# BucketMount

A small macOS app (Rust + Tauri 2) that mounts S3 buckets as volumes, keeps
them mounted, and tells you from the menu bar when the connection is lost.

- Each bucket appears as a volume in Finder (under **Locations** in the
  sidebar). Files you save are uploaded within seconds; files others upload
  appear within a minute.
- A menu bar dot shows the worst state of all mounts: green connected, blue
  syncing, orange mounting, red disconnected/down, grey disabled.
- The `rclone nfsmount` process behind each volume is babysat: if it dies,
  hangs, is ejected, or the bucket becomes unreachable, you get a
  notification and it is restarted with backoff. Nothing needs installing —
  rclone ships inside the app and macOS's built-in NFS client does the mounting
  (no kernel extensions, no macFUSE).
- Everything is driven by one file, `~/.config/bucketmount/config.toml`.
  Copy it to a new Mac, launch the app, and your buckets come back.

## Install

1. Download `BucketMount_<version>.dmg` (or the `.zip`) from the
   [Releases](../../releases) page and drag **BucketMount.app** to
   `/Applications`.
2. Open it. Because the app is not notarized by Apple, macOS will refuse the
   first launch. Go to **System Settings → Privacy & Security**, scroll down,
   and click **Open Anyway** next to BucketMount, then launch it again.
   (Alternatively: `xattr -dr com.apple.quarantine /Applications/BucketMount.app`.)
3. On first launch it asks whether to start at login. Say yes if you want your
   volumes available whenever you log in. A "Background Items Added"
   notification from macOS is expected; the entry appears under
   **System Settings → General → Login Items**.
4. Click **Add mount**, enter the bucket, region and credentials, hit **Test
   connection**, then **Save**. The volume mounts within a few seconds.

Requirements: macOS 13 or newer, Apple silicon or Intel.

## The two views

**Mount list** — one card per bucket with its state, mount point, an **Open**
button (reveals the volume in Finder) and **Edit**. Below the list: the
start-at-login switch, the rclone version in use, and links to the config and
log folders.

**Mount settings** — name, bucket, optional path inside the bucket, mount
point, provider/region/endpoint, credentials (inline keys, the AWS default
credential chain, or an existing rclone remote), enabled/read-only, and an
**Advanced** section for the upload delay, listing cache, cache size and extra
rclone flags. When editing an existing mount the bottom of the view shows its
live status, restart count, a **Restart mount** button and the tail of its
rclone log.

## Configuration file

`~/.config/bucketmount/config.toml` — see [`config.example.toml`](config.example.toml)
for every field with comments. Minimal example:

```toml
start_at_login = true

[[mount]]
name = "photos"
bucket = "my-photos-bucket"
mount_point = "~/BucketMount/photos"
region = "us-west-2"
access_key_id = "AKIA..."
secret_access_key = "..."
```

The UI writes this file; you can also edit it by hand and relaunch the app.
Credentials are stored in plain text with file mode `0600`. If you would
rather not keep keys in the file, use `env_auth = true` (AWS CLI profiles, SSO,
instance roles) or `rclone_remote = "name"` to reuse a remote from your own
`~/.config/rclone/rclone.conf`.

Set `BUCKETMOUNT_CONFIG_DIR` to use a different directory.

## How it works

```
BucketMount.app (Rust core, Tauri 2 window + menu bar item, plain HTML/CSS/JS UI)
 └─ one supervisor thread per mount
     └─ rclone nfsmount  bucket:name  ~/BucketMount/name
        --vfs-cache-mode full  --vfs-write-back 5s  -o locallocks  --rc
```

- **Mounting.** rclone runs a local NFS server for the bucket and mounts it
  with macOS's `mount_nfs`. Reads are streamed and cached on demand; writes
  land in a local cache (`~/Library/Caches/BucketMount/<name>`) and are
  uploaded `write_back_secs` after the last write. `-o locallocks` makes
  `flock()` work on the volume so editors, SQLite, cargo and the like behave.
- **Watching.** Every second the supervisor checks the process and the mount
  table. Every 5 s it queries rclone's remote-control API for the upload queue
  (this drives the *Syncing* state). Every `health_check_interval_secs` it
  performs a real listing request against the bucket; failure flips the mount
  to *Connection lost* and a macOS notification is posted. Checks run more
  often while disconnected so recovery is noticed quickly.
- **Recovering.** If rclone exits, stops answering for 90 s, or the volume is
  ejected, the supervisor force-unmounts any stale volume and restarts with
  exponential backoff (2 s … 60 s, reset after 5 minutes of health). On
  startup, leftovers from a previous run (orphaned rclone processes, stale
  mounts) are cleaned up first.
- **Staying alive.** The login item is a LaunchAgent with
  `KeepAlive.SuccessfulExit = false`, so launchd relaunches the app if it ever
  crashes, but not when you quit it. Quitting (or logout) unmounts all
  volumes cleanly. Only one instance runs at a time.
- **Logs.** `~/Library/Logs/BucketMount/app.log` for the app,
  `<name>.log` for each mount's rclone output (rotated at 10 MB).

## Notes and limitations

- Finder writes `._*` AppleDouble files next to files on network volumes; they
  are harmless but will appear in the bucket.
- S3 has no change notifications, so changes made elsewhere show up after at
  most `dir_cache_secs` (default 60 s). Lower it for faster visibility at the
  cost of more listing requests.
- Two machines writing the same file at the same time will not be merged; the
  last upload wins.
- Mount points must be empty folders. `/Volumes` is not writable without
  root, so the default is `~/BucketMount/<name>`; volumes still appear in the
  Finder sidebar.

## Building from source

Requires Rust (stable), Xcode command line tools and the Tauri CLI
(`cargo install tauri-cli --version "^2" --locked`). No Node.js: the front end
in `ui/` is plain HTML, CSS and JavaScript loaded straight by the WebView.

```sh
scripts/build-app.sh                 # universal .app + .dmg + .zip with rclone bundled -> dist/
ARCHS=native scripts/build-app.sh    # quicker, current architecture only
cargo tauri dev                      # run with live-reloading front end (needs binaries/ populated once by the script)
```

The build script downloads the pinned rclone release into `binaries/` (Tauri's
`externalBin` places it inside the bundle as `Contents/MacOS/rclone`), runs
`cargo tauri build`, ad-hoc signs the result when no Developer ID is
configured, and collects the outputs. If the checkout lives on a network mount
(cargo cannot take file locks there) it mirrors the sources to
`~/.cache/bucketmount/src` first and puts `dist/` there too.

`.github/workflows/release.yml` does the same on a GitHub runner whenever a
`v*` tag is pushed and attaches the zip and dmg to a release. Add the
`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`,
`APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID` secrets and the Tauri bundler
signs and notarizes the build so it opens without the Gatekeeper detour.

Layout: `src/` Rust (config, rclone, supervisor, mac helpers, Tauri commands
and tray), `ui/` front end, `tauri.conf.json` window/bundle settings,
`capabilities/` Tauri permissions, `icons/` app icon (regenerate from
`assets/icon.png` with `scripts/gen-icon.py` + `iconutil`).

## License

MIT
