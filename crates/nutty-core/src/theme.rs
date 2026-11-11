//! Shared color tokens for chrome and terminal rendering.

use crate::config::Appearance;

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub base: u32,
    pub sidebar: u32,
    pub surface: u32,
    pub hover: u32,
    pub border: u32,
    pub text: u32,
    pub muted: u32,
    pub accent: u32,
    pub success: u32,
    pub terminal: u32,
    pub ansi: [u32; 16],
}

impl Palette {
    pub fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Graphite => Self {
                base: 0x101216,
                sidebar: 0x15171c,
                surface: 0x1b1e25,
                hover: 0x242832,
                border: 0x292d36,
                text: 0xe4e7ee,
                muted: 0x858c9e,
                accent: 0xb7a3ff,
                success: 0x86d9b1,
                terminal: 0x101216,
                ansi: [
                    0x242832, 0xf17c82, 0x86d9b1, 0xf2ce89, 0x8faaf4, 0xc6a2f7, 0x8bd5df, 0xd9dee9,
                    0x697386, 0xff9da4, 0xa5edc9, 0xffdda7, 0xb0c4ff, 0xe0baff, 0xa8edf4, 0xf5f6fa,
                ],
            },
            Appearance::Light => Self {
                base: 0xffffff,
                sidebar: 0xf6f7fa,
                surface: 0xf0f2f6,
                hover: 0xe7eaf1,
                border: 0xe0e3eb,
                text: 0x262b38,
                muted: 0x788193,
                accent: 0x7358ce,
                success: 0x2d9670,
                terminal: 0xffffff,
                ansi: [
                    0x262b38, 0xba3b4b, 0x268565, 0xa37520, 0x3e62b5, 0x8352b0, 0x277e8e, 0xb7becd,
                    0x657087, 0xd45563, 0x389879, 0xb88730, 0x5879cc, 0x9c68c7, 0x409ba8, 0xe0e3eb,
                ],
            },
            Appearance::Solarized => Self {
                base: 0x002b36,
                sidebar: 0x073642,
                surface: 0x0b3e49,
                hover: 0x144752,
                border: 0x1a4b56,
                text: 0x93a1a1,
                muted: 0x657b83,
                accent: 0x2aa198,
                success: 0x859900,
                terminal: 0x002b36,
                ansi: [
                    0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                    0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
                ],
            },
        }
    }
}
