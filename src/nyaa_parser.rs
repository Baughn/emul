use scraper::{Html, Selector};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum NyaaParserError {
    #[error("Could not find the magnet link anchor tag in the HTML content")]
    MagnetLinkNotFound,
    #[error("Failed to parse CSS selector: {0}")]
    SelectorParseError(String),
    #[error("Found magnet link tag, but it is missing the 'href' attribute")]
    HrefAttributeMissing,
}

/// Extracts the primary magnet link from the HTML content of a Nyaa.si view page.
///
/// It looks for an anchor tag (`<a>`) whose `href` attribute starts with "magnet:?".
/// Assumes the input HTML is from a single torrent's page, not a search results page.
///
/// # Arguments
///
/// * `html_content` - A string slice containing the HTML source of the Nyaa page.
///
/// # Returns
///
/// A `Result` containing the magnet URL as a `String` if found, or a `NyaaParserError` otherwise.
pub fn extract_magnet_url(html_content: &str) -> Result<String, NyaaParserError> {
    let document = Html::parse_document(html_content);

    // CSS selector for the magnet link. Nyaa typically uses an <a> tag
    // where the href starts with "magnet:?".
    // Using a raw string `r#""#` is good practice for selectors.
    let selector_str = r#"a[href^="magnet:?"]"#;
    let magnet_selector = Selector::parse(selector_str)
        .map_err(|e| NyaaParserError::SelectorParseError(e.to_string()))?;

    // Find the first element matching the selector.
    // On a Nyaa view page, there should ideally be only one main magnet link.
    if let Some(element) = document.select(&magnet_selector).next() {
        // Extract the 'href' attribute value.
        if let Some(href) = element.value().attr("href") {
            Ok(href.to_string())
        } else {
            // This should be rare if the selector matched, but handle it defensively.
            Err(NyaaParserError::HrefAttributeMissing)
        }
    } else {
        // No element matched the selector.
        Err(NyaaParserError::MagnetLinkNotFound)
    }
}

// --- Unit Tests ---
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Helper function to load test HTML data
    fn load_test_html(filename: &str) -> String {
        fs::read_to_string(filename)
            .unwrap_or_else(|e| panic!("Failed to read test file {}: {}", filename, e))
    }

    #[test]
    fn test_extract_magnet_from_real_file() {
        let html_content = load_test_html("testdata/nyaa.html");
        let expected_magnet = "magnet:?xt=urn:btih:ABCDEFGHIJKLMNOPQRSTUVWXYZ123456&dn=Some%20Torrent&tr=http%3A%2F%2Fnyaa.tracker.wf%3A7777%2Fannounce&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&tr=udp%3A%2F%2Ftracker.internetwarriors.net%3A1337%2Fannounce&tr=udp%3A%2F%2Ftracker.leechers-paradise.org%3A6969%2Fannounce&tr=udp%3A%2F%2Ftracker.coppersurfer.tk%3A6969%2Fannounce";
        match extract_magnet_url(&html_content) {
            Ok(url) => assert_eq!(url, expected_magnet),
            Err(e) => panic!("Extraction failed: {}", e),
        }
    }

    #[test]
    fn test_no_magnet_link() {
        let html_content = r#"
        <!DOCTYPE html>
        <html><body><p>No magnet link here.</p></body></html>
        "#;
        match extract_magnet_url(html_content) {
            Err(NyaaParserError::MagnetLinkNotFound) => (), // Expected error
            Ok(url) => panic!("Expected error, but got URL: {}", url),
            Err(e) => panic!("Expected MagnetLinkNotFound, but got different error: {}", e),
        }
    }

    #[test]
    fn test_magnet_link_no_href() {
        // Highly unlikely scenario, but good to test selector robustness
        let html_content = r#"
        <!DOCTYPE html>
        <html><body><a>This link is missing href</a></body></html>
        "#;
        // This test expects MagnetLinkNotFound because the selector `a[href^="magnet:?"]`
        // specifically requires the href attribute to exist and start with "magnet:?".
        // If the selector was just "a", then we might expect HrefAttributeMissing.
        match extract_magnet_url(html_content) {
            Err(NyaaParserError::MagnetLinkNotFound) => (), // Expected error
            Ok(url) => panic!("Expected error, but got URL: {}", url),
            Err(e) => panic!("Expected MagnetLinkNotFound, but got different error: {}", e),
        }
    }

     #[test]
    fn test_magnet_link_no_href_but_tag_exists() {
        // Test case where the tag exists but lacks the href attribute
        // We need to adjust the test HTML slightly to make the tag match *something*
        // that looks like the target but is broken. Let's simulate a tag that
        // might be selected if the selector was less specific, but ensure our
        // specific selector doesn't find it.
         let html_content = r#"
         <!DOCTYPE html>
         <html><body><a class="magnet-link-style-but-no-href">Click me maybe?</a></body></html>
         "#;
         // The specific selector `a[href^="magnet:?"]` will not match this.
         match extract_magnet_url(html_content) {
             Err(NyaaParserError::MagnetLinkNotFound) => (), // Correct, selector didn't match
             Ok(url) => panic!("Expected error, but got URL: {}", url),
             Err(e) => panic!("Expected MagnetLinkNotFound, but got different error: {}", e),
         }

         // Let's test the HrefAttributeMissing case directly, although it's hard
         // for the current selector to trigger it. Imagine a hypothetical scenario
         // where the parser somehow selected an `<a>` tag that matched the selector pattern
         // but the attribute retrieval failed (e.g., malformed HTML or parser bug).
         // We can't easily simulate this perfectly without controlling the parser internals,
         // but the error type exists for robustness.
    }

    #[test]
    fn test_invalid_selector() {
        // This test is more about the internal robustness, ensuring Selector::parse errors are handled.
        // We can't directly call extract_magnet_url with an invalid selector easily,
        // but we know the error type exists. If the constant selector string were somehow
        // made dynamic and invalid, this error path would be relevant.
        let invalid_selector = "[invalid";
        let result = Selector::parse(invalid_selector);
        assert!(result.is_err());
        // We can simulate the error mapping part
        let mapped_error = result.map_err(|e| NyaaParserError::SelectorParseError(e.to_string()));
        assert!(matches!(mapped_error, Err(NyaaParserError::SelectorParseError(_))));
    }
}
