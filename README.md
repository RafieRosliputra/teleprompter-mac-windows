# JohorStudio Teleprompter

Two-screen presidential teleprompter written in Rust (egui). macOS now, Windows later.

- **Main screen (operator):** left 2/3 = script (click a line to glide it to the reading line), right 1/3 = live preview of screen 2 + controls.
- **Screen 2 (prompter):** black background, white text, mirrored for the glass, reading line at 1/3 from the top with red cursors left and right.
- **Keys:** `Space` play at default speed / pause · `→` faster · `←` slower / reverse · `↑`/`↓` one line · `Home` top · `Esc` stop.
- Scripts can be loaded from a `.txt` file or from **tele.johorstudio.com** with a 6-digit code.

## Download (Mac)

Get `JohorStudio-Teleprompter-mac-arm64.zip` from [Releases](../../releases), unzip, move the app to Applications.
The app is not notarized: the first time, right-click the app → **Open** → **Open**
(or run `xattr -dr com.apple.quarantine "/Applications/JohorStudio Teleprompter.app"`).

## Phone remote

Click **📱 Remote** (QR card, bottom right) and scan it with the phone:

- **Internet**: `https://tele.johorstudio.com/r/<session>`. The app connects out to the relay on the script
  server, so the phone needs no Tailscale and can be on any network. HTTPS, so keep-awake and fullscreen work.
  "⟳ Jana QR baru" makes a new session (old phones are cut off).
- **Tempatan**: `http://<mac-ip>:8140` (Tailscale 100.x or the same Wi-Fi; WebSocket on 8141). Works without internet.

The phone draws the same rows as screen 2. Drag the preview to move the text, tap a line to glide to it.

## Build

```sh
cargo build --release              # binary in target/release
./scripts/make-mac-app.sh install  # build .app into dist/ and copy to /Applications
```

## Script server (tele.johorstudio.com)

`server/` is a small Python (stdlib only) web app + SQLite: the public submits scripts and gets a code by email;
the Mac app lists/loads/deletes them with the admin token. A web admin at `/admin` (list, search, delete one or many)
is protected by Cloudflare Access, and the server also verifies the Access JWT itself. It runs as the TrueNAS custom app `teleprompter`
(see `deploy/truenas-app.yaml`, data in `/mnt/ssd/teleprompter`), published through the Cloudflare tunnel.
