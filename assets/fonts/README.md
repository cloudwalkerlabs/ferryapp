# Instrument Sans

Regular and Bold are the unmodified static TTF faces from
https://github.com/Instrument/instrument-sans/tree/master/fonts/ttf.
The website uses the matching static WOFF2 faces from `fonts/webfonts`.
Both are licensed under the SIL Open Font License 1.1; see `OFL.txt`.

The app embeds both faces in `src/ui/widgets.rs`. Keep Regular (400) and
Bold (700) together so cosmic-text can match the exact weight. Other
scripts fall back to system fonts; pairing codes use system monospace.
