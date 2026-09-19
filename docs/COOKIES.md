# Browser Cookie Extraction (Windows)

Win-CodexBar can extract browser cookies for providers that use web authentication (Claude, Cursor, Kimi, and others). This is the Windows rewrite of upstream cookie/Keychain concepts: **DPAPI + browser profiles**, not macOS Keychain prompts.

## Supported Browsers

| Browser | Encryption | Status |
|---------|-----------|--------|
| Chrome | DPAPI + AES-256-GCM; modern profiles may use Chromium ABE (`v20`) | ⚠️ Automatic only when the needed cookies are not App-Bound |
| Edge | DPAPI + AES-256-GCM; modern profiles may use Chromium ABE (`v20`) | ⚠️ Automatic only when the needed cookies are not App-Bound |
| Brave | DPAPI + AES-256-GCM; modern profiles may use Chromium ABE (`v20`) | ⚠️ Automatic only when the needed cookies are not App-Bound |
| Firefox | Unencrypted SQLite | ✅ Automatic |

Chromium App-Bound Encryption (ABE) binds protected cookie keys to the browser installation. Win-CodexBar does not bypass that protection. If the selected Chromium profile stores the provider cookies as App-Bound `v20` values, automatic import cannot decrypt them with the normal user DPAPI key. Use a manual Cookie header or Firefox instead. Chromium browser choices remain available because older or unmigrated profiles can still contain readable DPAPI/AES-GCM cookies.

## How It Works

1. CodexBar reads the browser's cookie database from its standard location
2. For Chromium-based browsers, CodexBar can decrypt legacy DPAPI/AES-GCM cookies using the current user's credentials; App-Bound `v20` cookies are intentionally not bypassed
3. Only cookies for enabled providers are extracted (e.g., `claude.ai`, `cursor.com`)
4. Cookies are stored in-memory and refreshed on each provider poll

## Setting Up Cookie Import

1. Open **Settings** → **Providers** tab
2. Select the provider you want to configure
3. In the provider detail pane, find the **Browser Cookies** section
4. Choose your browser from the dropdown and click **Import**

## Manual Cookies

If automatic extraction fails (for example, Chromium App-Bound Encryption is active, the browser database cannot be read, or CodexBar is running in WSL):

1. Open your browser and navigate to the provider's website (e.g., `claude.ai`)
2. Open DevTools (F12) → **Network** tab
3. Refresh the page and click any request to the provider
4. Copy the `Cookie` header value from **Request Headers**
5. In CodexBar Settings → provider detail → **Browser Cookies**, paste the value

## Troubleshooting

- **"Chromium App-Bound Encryption"**: Modern Chrome, Edge, Brave, and other Chromium-based profiles can protect cookies with ABE. Closing the browser does not remove ABE; use a manual Cookie header or Firefox for the same login
- **"Cookie decryption failed"**: Close the browser and retry if the cookie database itself is locked
- **Empty cookies**: Make sure you're logged into the provider's web interface in that browser
- **WSL**: Chromium DPAPI cookies cannot be decrypted from WSL. Use manual cookies or CLI-based auth instead

## Related

- [CONFIGURATION.md](./CONFIGURATION.md) — where manual cookies and settings live on disk
- [PROVIDERS.md](./PROVIDERS.md) — web vs cli vs oauth sources
- [WSL.md](./WSL.md) — why automatic Chromium decrypt fails in WSL
