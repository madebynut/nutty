# Nutty icon

Black rounded tile, white acorn, and a black `>_` cut into the shell. Flat artwork, with no gradients, shadows or font dependencies. The margin outside the tile is transparent.

`Nutty.svg` is the editable vector source. `Nutty.png` is the 1024 px export. `Nutty.icns` includes all ten macOS representations from 16 px through 1024 px. `Nutty-preview.png` shows the icon against light/dark backgrounds and at 16/32/64 px.

The terminal mark uses a 32-unit stroke. `Nutty-workspace.png` is a 512 px export embedded in the app for the empty workspace; its decoded RGBA image uses 1 MiB and shares GPUI's image cache.

`Nutty.ico` contains seven PNG-backed Windows sizes from 16 to 256 px, with resource ID 1.

Regenerate PNG, ICO and ICNS on macOS:

```sh
swift scripts/generate-icon.swift
```

The bundle script regenerates the icon when the vector source or renderer changes. Every size is rendered from the vector source, independent of display scaling.
