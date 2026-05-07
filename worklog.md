---
Task ID: 1
Agent: main
Task: Fix CDN 403 Forbidden error through Tor SOCKS5 proxy

Work Log:
- Analyzed the SOCKS5 forwarder code and identified the root cause of CDN 403
- The SOCKS5 greeting was offering BOTH no-auth (0x00) and username/password (0x02) methods
- When Tor's IsolateSOCKSAuth is enabled and both methods are offered, Tor may choose no-auth
- If Tor chooses no-auth, the isolation username is NEVER sent → different Tor circuit → different exit IP → CDN 403
- The resolver (reqwest) only offers username/password auth when a SOCKS5 URL includes a username
- Fixed the SOCKS5 greeting to only offer username/password auth (0x02), matching reqwest's behavior
- Added Sec-Fetch-* headers to souphttpsrc (Sec-Fetch-Dest: video, Sec-Fetch-Mode: no-cors, Sec-Fetch-Site: cross-site)
- Changed Accept header from restrictive "video/webm,video/mp4,..." to "*" to avoid CDN rejection
- Added diagnostic logging for SOCKS5 connection establishment
- Committed and pushed to GitHub as e4c2fd0

Stage Summary:
- Key fix: SOCKS5 greeting changed from [0x05, 0x02, 0x00, 0x02] to [0x05, 0x01, 0x02]
- This guarantees Tor uses the isolation username → same circuit as resolver → same exit IP
- Added Sec-Fetch-* headers for CDN anti-bot compatibility
- User needs to `git pull && ./deploy.sh` on the Pi to test

---
Task ID: 2
Agent: main
Task: Add missing Voe front-end domains to resolver

Work Log:
- Identified that brittanyaheadnew.com was not in VOE_DOMAINS list
- This caused the resolver to fall through to yt-dlp instead of custom Voe resolver
- yt-dlp cannot handle Voe's obfuscation → fails → session enters error state
- The SOCKS5 auth fix from Task 1 was never even tested because the Voe custom resolver wasn't triggered
- Added brittanyaheadnew.com and 8+ other common Voe front-end domains
- Committed and pushed as d37cfb7

Stage Summary:
- Root cause of the new failure: domain mismatch, not SOCKS5 issue
- With the domain now in the list, the custom Voe resolver will be used
- This means the SOCKS5 auth fix can be properly tested on next deploy
- User needs to `git pull && ./deploy.sh` on the Pi to test
