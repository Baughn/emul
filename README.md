# EmulBot - Vorpal Bunny IRC Companion

EmulBot is an IRC bot inspired by the character Emul from Shangri-La Frontier. It acts as a helpful and sometimes quirky chat companion, leveraging Google's Gemini AI for natural language understanding and response generation.

## Features

*   **AI Chat:** Responds to direct mentions and occasionally interjects into conversations using Google Gemini.
*   **Personality:** Modeled after Emul, a Vorpal Bunny guide NPC. (See `vorpal_bunny_prompt.txt`)
*   **Tool Use:** Can perform actions requested by users or the AI, including:
    *   Rolling dice (e.g., "roll 3d6+2")
    *   Initiating torrent downloads from Nyaa.si URLs.
    *   Fetching and processing images from URLs for the AI to analyze.
*   **Persistence:** Remembers channels to join and admin users using an SQLite database.
*   **Message Logging:** Logs channel messages for context.
*   **Admin Commands:** Allows administrators to manage channels and admins via private messages.
*   **Configurable:** Settings managed via command-line arguments and environment variables.
*   **Blue Noise Interjections:** Uses a blue noise algorithm for more natural-feeling random interjections.

## Setup

1.  **Rust:** Ensure you have a recent Rust toolchain installed (https://rustup.rs/).
2.  **API Key:** Obtain a Google AI Gemini API key (https://aistudio.google.com/app/apikey).
3.  **Environment Variables:** Create a `.env` file in the project root directory with your API key:

    ```dotenv
    GEMINI_API_KEY=YOUR_API_KEY_HERE
    # Optional: Set NickServ password if needed
    # NICKSERV_PASSWORD=YOUR_NICKSERV_PASSWORD
    # Optional: Set the default admin nick if different from 'Baughn'
    # EMUL_BOT_ADMIN=YourAdminNick
    ```

4.  **Build:** Compile the bot:
    ```bash
    cargo build --release
    ```

## Configuration & Running

The bot requires several command-line arguments to run:

```bash
./target/release/emul --server <irc.server.address> --db <path/to/database.sqlite>
```

**Required Arguments:**

*   `--server <address>`: The hostname or IP address of the IRC server.
*   `--db <path>`: Path to the SQLite database file (will be created if it doesn't exist).

**Optional Arguments:**

*   `--port <port>`: IRC server port (default: 6697 for TLS).
*   `--nickname <nick>`: Bot's nickname (default: "Emul").
*   `--admin <nick>`: Nickname of the initial administrator (default: "Baughn", can also be set via `EMUL_BOT_ADMIN` env var).
*   `--nickserv-password <password>`: NickServ password (can also be set via `NICKSERV_PASSWORD` env var).
*   `--use-tls <true|false>`: Whether to use TLS (SSL) for the connection (default: true). Use `--use-tls false` for non-SSL connections (e.g., port 6667).

**Example:**

```bash
./target/release/emul --server irc.libera.chat --db emul_memory.sqlite --nickname VorpalBot --admin MyAdminNick
```

## Running Tests

Some tests require network access and a valid `GEMINI_API_KEY` in the `.env` file. These tests are marked with `#[ignore]` by default.

*   Run all tests (excluding ignored):
    ```bash
    cargo test
    ```
*   Run all tests including ignored (requires network and API key):
    ```bash
    cargo test -- --ignored
    ```

## Admin Commands

Send these commands to the bot via private message (PM/Query):

*   `!join #channel`: Adds the channel to the auto-join list and joins it.
*   `!part #channel`: Removes the channel from the auto-join list and parts it.
*   `!add_admin <nickname>`: Grants admin privileges to the specified nickname.
*   `!del_admin <nickname>`: Revokes admin privileges from the specified nickname.
*   `!admins`: Lists all registered admin nicknames.
*   `!channels`: Lists all channels the bot is set to auto-join.
*   `!interject`: Forces the bot to try and interject on the next message in any channel.
*   `!help`: Shows the list of admin commands.

## Contributing

Contributions are welcome! Please feel free to open issues or pull requests.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
```
