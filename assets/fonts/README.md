# Inter

Regular (400) and Bold (700) are the unmodified static TTF faces from
`extras/ttf` in the official Inter 4.1 release:
https://github.com/rsms/inter/releases/tag/v4.1.
The website uses the matching WOFF2 faces from the release’s `web` directory.
Both are licensed under the SIL Open Font License 1.1; see `OFL.txt`.

The app embeds both faces in `src/ui/widgets.rs`, with Normal (400) as the
default text weight. Keep Regular and Bold together so cosmic-text can match
the exact weight. Scripts outside Inter’s coverage fall back to system
fonts; pairing codes use system monospace.
