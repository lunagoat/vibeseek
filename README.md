# vibeseek

Soulseek from the terminal. A CLI + TUI frontend for [slskd](https://github.com/slskd/slskd).

- **Search** the network, with quality filters (lossless-first by default, like your sockseek config)
- **Download** single files or whole folders into any folder you choose
- **CSV / playlist batch mode**: CSVs, Spotify playlists/albums/liked songs, YouTube playlists; resumable, with automatic retry from other sources
- **Upload monitoring**: who's downloading from you, live progress and speed, permanent history and stats, cancel and ban

```
vibeseek                      # full-screen TUI (tabs: Search / Downloads / Uploads / History / Messages)
vibeseek search take five     # one-off search, numbered results
vibeseek get 1 3-5 -o ~/Music/jazz --wait
vibeseek search -a miles davis kind of blue   # group by folder (albums); `get 1` downloads the album
vibeseek uploads -w           # live view of people downloading from you
vibeseek history --since 7d   # upload stats: top downloaders, most-downloaded files, recent
vibeseek csv ~/Desktop/liked_songs.csv -o ~/Music/liked --fallback lossy-ok
```


## Installing on another computer (AppImage)

Send your friend `dist/vibeseek-<version>-x86_64.AppImage`. It runs on any 64-bit Linux with glibc 2.28 or newer (Ubuntu 20.04+, Debian 10+, Fedora 29+, Arch, …) and systemd.

1. Make it executable (`chmod +x vibeseek-*.AppImage`, or tick "allow executing" in the file manager), then double-click it or run it from a terminal. A double-click opens a terminal automatically.
2. The first run starts `vibeseek setup`, which asks for:
   - a Soulseek username and password (new names are registered on first login)
   - folders to share
   - where downloads go
   - how peers reach them: UPnP (automatic), ProtonVPN port forwarding, or a port they forward themselves
3. Setup then downloads slskd from its official GitHub release, writes the configs, copies vibeseek to `~/.local/bin/vibeseek`, starts the background services, and checks that the login and port work.

After that they just run `vibeseek`. UPnP needs the `miniupnpc` package (setup says so if it's missing). `vibeseek setup --uninstall` removes everything setup installed, but keeps downloads.

To build the AppImage: `./packaging/build-appimage.sh`. The first build downloads zig, cargo-zigbuild and appimagetool into `.tools/`.

## How it fits together

```
 vibeseek (CLI / TUI) ──HTTP──▶ slskd (systemd user service) ──▶ Soulseek network
        │                          ▲
        └── vibeseek agent ────────┘  keeps the VPN port in sync, records upload history,
            (systemd user service)    moves finished downloads into custom folders
```

| Thing | Where |
|---|---|
| vibeseek binary | `~/.local/bin/vibeseek` |
| vibeseek config | `~/.config/vibeseek/config.toml` (`vibeseek config --edit`) |
| slskd binary | `~/.local/opt/slskd/` (v0.26.0) |
| slskd config | `~/.local/share/slskd/slskd.yml` (credentials, shares, port, bans) |
| slskd service | `systemctl --user … slskd` or `vibeseek daemon start/stop/restart/status/logs` |
| agent service | `vibeseek-agent.service` (`vibeseek agent install/uninstall`) |
| upload history | `~/.local/share/vibeseek/history.db` |
| CSV progress | `~/.local/share/vibeseek/csv/` |
| slskd web UI | http://127.0.0.1:5030 (localhost only; login is in slskd.yml) |

## Search and download

`vibeseek search <words>` prints numbered results and remembers them, so `vibeseek get <numbers>` downloads them. You can also do both at once with `-d 1 -o <folder>`.

- `-p lossless | lossy-ok | any | video | dsd` picks a filter preset, and `--min-bitrate`, `--min-bitdepth`, `--min-samplerate`, `--strict` override it
- `-f` limits file types: extensions (`-f mkv`, `-f dsf,flac`) or groups (`video`, `dsd`, `lossless`, `lossy`, `audio`). In the TUI, press `t`. Audio quality limits only apply to audio files, so `-f mkv` isn't filtered by bit depth, and DSD's 1-bit files aren't rejected.
- `-a` groups results by folder, so `get` downloads the entire remote folder, cover art included
- `-o <folder>` downloads anywhere; without it, files go to `~/Music/downloads/<source folder>/`
- Ranking prefers hi-res FLAC (your sockseek `pref-*` values, in `[prefs]` in config.toml), peers with a free slot and short queue, and filenames that match your words

In the TUI, press `/` to search and `Enter` to download. `v` toggles the files/folders view, `f` cycles filters, and `o` sets the output folder. Press `?` for all keys.

## CSV and playlist batch mode

```
vibeseek csv liked_songs.csv                  # → ~/Music/vibeseek/liked_songs/
vibeseek csv liked_songs.csv --dry-run -n 20  # see what it would pick
vibeseek csv liked_songs.csv --status         # progress + list of what wasn't found
vibeseek csv liked_songs.csv --retry --fallback lossy-ok
```

- Columns are auto-detected (`title/track/name`, `artist/artists`, `album`, `length/duration/duration_ms`). You can override them with `--title-col` etc.
- Rows with only an album become album downloads (whole folder into `Artist - Album/`), and `--albums` forces that for every row.
- A match needs every title word in the filename, the artist somewhere in the path, and a length within 3s. Remixes and live versions are skipped unless the title asks for them.
- A failed or stuck transfer (queued for 15 minutes without starting) moves on to the next best source, up to 3 tries.
- It's safe to Ctrl-C and rerun: finished rows, and files already in the output folder, are skipped.
- Soulseek bans clients that search too fast, so it stays under 34 searches per 220s. A 2,500-row CSV takes about 4.5 hours.

### Where the files go

Each CSV or playlist gets its own folder under `~/Music/vibeseek/` (`csv.output_root` in config.toml), named after the CSV file or playlist. Rerunning the same input always reuses its folder:

- A CSV is identified by its path. A link is identified by the playlist, album or video it points to, so re-copied share links (`?si=…`) count as the same input.
- The folder is remembered in the input's progress file, and the folder carries a hidden `.vibeseek-source` tag. If two different playlists have the same name, the second gets a short ID appended: `Chill [3fa9c1]`.
- `-o <folder>` puts a run somewhere else, and that choice is remembered for later runs.

### Spotify and YouTube links

Instead of a CSV you can pass a link; everything else works the same (filters, resume, `--status`, `--retry`). `vibeseek playlist` is an alias for `vibeseek csv`.

```
vibeseek playlist "https://open.spotify.com/playlist/…"       # needs `vibeseek spotify login` once
vibeseek playlist spotify-likes                                # your Liked Songs (needs login)
vibeseek playlist "https://open.spotify.com/album/…"           # albums/tracks work without login
vibeseek playlist "https://www.youtube.com/playlist?list=…"    # YouTube / YouTube Music, via yt-dlp
```

- **Spotify:** uses the developer app in `[spotify]` in config.toml. Spotify only shows playlist contents to logged-in users, so run `vibeseek spotify login` once. It opens your browser, and the login is remembered (`vibeseek spotify logout` forgets it). The app must have `http://127.0.0.1:8888/callback` as a Redirect URI.
- **YouTube:** video titles like "Artist - Song (Official Video) [4K]" become artist "Artist", title "Song". For "Artist - Topic" auto-generated channels, the channel is the artist.
- The link is fetched again on every run, so tracks added to the playlist later are picked up when you rerun it. Files go to `~/Music/vibeseek/<playlist name>/` unless you pass `-o`.


## Uploads

`vibeseek uploads` shows active and queued uploads (`-w` for a live view, `--all` for finished ones too). `vibeseek uploads cancel 2`, `… cancel user:NAME`, `vibeseek ban NAME`, and `vibeseek uploads clear` manage them.

slskd forgets finished transfers after a while, so the agent (and the TUI while it's open) records every finished upload into `history.db`. `vibeseek history` shows totals, top downloaders, most-downloaded files, and recent uploads. `-u NAME` shows one person's uploads.

## Staying reachable (VPN port / router port)

Peers have to be able to connect to you, or searches come back empty and downloads time out. Every 20s the agent picks a route:

- **VPN up** (`proton0` exists): it asks the VPN gateway for the forwarded port (`natpmpc -g 10.2.0.1`) and uses that. ProtonVPN hands out a new port on every connect.
- **VPN down:** it opens port **50300** on your home router via UPnP and uses that, the same way Nicotine+ does. Peers see your home IP while you're in this mode.

Whenever the route or port changes, the agent writes the port into slskd.yml (slskd applies it live) and reconnects slskd to the Soulseek server. That way the server learns the new port, and any connection that died with the VPN gets replaced. When the VPN comes back, the router port is closed again.

- `vibeseek port --show` shows the active route and whether it's applied
- `vibeseek port` syncs right now, and `vibeseek port 40649` sets a port by hand (the agent re-syncs later)
- Every 30 minutes (`port.check_minutes`), and a minute after any route change, the agent runs Soulseek's port test. If you've become unreachable, it re-syncs the port, reconnects slskd, and sends a desktop notification (`port.notify`). It notifies you again when you're reachable. `vibeseek status` shows the last result. If the VPN stops forwarding a port for more than 10 minutes, you get a notification telling you to reconnect it, because nothing else fixes that.
- The settings are `[port]` in config.toml: `vpn_interface = "proton0"`, `upnp = true`, `upnp_port = 50300`, `auto = true`

## Messages

Tab **5** in the TUI shows your private messages. Conversations with unread messages are listed first, and the top bar shows `✉ N` for unread messages on every tab.

- Opening a conversation marks it read. Press `Enter` (or `r`) to reply, `n` to message someone new, `b` to ban the sender (good for spam), and `d` to close the conversation. A closed conversation comes back if they write again.
- From the command line: `vibeseek messages` lists conversations, `vibeseek messages <user>` shows one and marks it read, and `vibeseek msg <user> <text>` sends a message.

## Checking your port

`vibeseek port --check` asks Soulseek's own port tester (the page SoulseekQT's "Check ports" opens) whether peers can reach you, and prints the verdict. `vibeseek port --open` opens that page in your browser instead. In the TUI, press `P` on any tab. The result shows in the status line, and the port number in the top bar turns green or red.

The tester checks whichever IP the request comes from. That's your VPN address while the VPN is up and your home address otherwise, which is the same route slskd uses.

## Using sockseek / Nicotine+ too

Soulseek allows one login per account, and slskd holds it while it's running. Run `vibeseek daemon stop` before logging in to the same account with sockseek or Nicotine+, and `vibeseek daemon start` afterwards.
