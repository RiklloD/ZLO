//! Theme presets: IronClaw's own pair plus a short, curated list of
//! Zeron's built-in themes (MIT, see `THIRD_PARTY_NOTICES.md`) and popular
//! editor palettes. Every one is checked to keep the sidebar a step darker
//! than the content, so the layers read without hairlines;
//! [`super::Palette::from_seed`] derives every other token from these eleven
//! colors.

pub struct ThemeSeed {
    pub id: &'static str,
    pub name: &'static str,
    pub bg: u32,
    pub shell: u32,
    pub raised: u32,
    pub card: u32,
    pub text: u32,
    pub muted: u32,
    pub faint: u32,
    pub accent: u32,
    pub danger: u32,
    pub warning: u32,
    pub success: u32,
}

pub const DEFAULT_THEME: &str = "ironclaw";
pub const DEFAULT_LIGHT_THEME: &str = "ironclaw-light";

macro_rules! seeds {
    ($($id:literal $name:literal $bg:literal $shell:literal $raised:literal $card:literal
       $text:literal $muted:literal $faint:literal $accent:literal $danger:literal
       $warning:literal $success:literal;)*) => {
        pub const THEMES: &[ThemeSeed] = &[$(ThemeSeed {
            id: $id, name: $name, bg: $bg, shell: $shell, raised: $raised, card: $card,
            text: $text, muted: $muted, faint: $faint, accent: $accent, danger: $danger,
            warning: $warning, success: $success,
        }),*];
    };
}

seeds! {
    // IronClaw's own pair: graphite and paper, lit in ember.
    "ironclaw" "Numoh" 0x141417 0x0c0c0e 0x27272c 0x1c1c20 0xededef 0xa1a1aa 0x71717a 0xff8a4c 0xf87171 0xfbbf24 0x4ade80;
    "ironclaw-light" "IronClaw Light" 0xfdfdfc 0xf3f2ef 0xe9e8e4 0xffffff 0x1c1b1a 0x5f5e5a 0x8f8d88 0xe8590c 0xdc2626 0xb45309 0x15803d;
    // Dark, from Zeron (MIT) and each project's published palette.
    "zeron-dark" "Zeron Dark" 0x111113 0x080808 0x2c2c30 0x18181b 0xe8e8ea 0xa9a9ae 0x85858a 0x8b7cf6 0xf87171 0xfacc15 0x34d399;
    "catppuccin-mocha" "Catppuccin Mocha" 0x1e1e2e 0x181825 0x313244 0x242436 0xcdd6f4 0xa6adc8 0x9399b2 0xcba6f7 0xf38ba8 0xf9e2af 0xa6e3a1;
    "tokyo-night" "Tokyo Night" 0x1a1b26 0x16161e 0x292e42 0x1f2335 0xc0caf5 0x9aa5ce 0x7f87ad 0x7aa2f7 0xf7768e 0xe0af68 0x9ece6a;
    "rose-pine" "Rosé Pine" 0x191724 0x14121d 0x26233a 0x1f1d2e 0xe0def4 0x908caa 0x6e6a86 0xc4a7e7 0xeb6f92 0xf6c177 0x9ccfd8;
    "kanagawa-wave" "Kanagawa Wave" 0x1f1f28 0x16161d 0x363646 0x2a2a37 0xdcd7ba 0xc8c093 0x727169 0x7e9cd8 0xe46876 0xe6c384 0x98bb6c;
    "nord" "Nord" 0x2e3440 0x292e39 0x434c5e 0x3b4252 0xd8dee9 0xaeb6c4 0x7f899b 0x88c0d0 0xbf616a 0xebcb8b 0xa3be8c;
    // Light.
    "zeron-light" "Zeron Light" 0xffffff 0xf3f3f5 0xededf0 0xffffff 0x303035 0x62626a 0x797981 0x5b43e8 0xdc2626 0xa16207 0x15803d;
    "catppuccin-latte" "Catppuccin Latte" 0xeff1f5 0xe6e9ef 0xdce0e8 0xf7f8fa 0x4c4f69 0x5c5f77 0x6c6f85 0x8839ef 0xd20f39 0xdf8e1d 0x40a02b;
    "rose-pine-dawn" "Rosé Pine Dawn" 0xfaf4ed 0xf2e9e1 0xdfdad9 0xfffaf3 0x575279 0x797593 0x9893a5 0x907aa9 0xb4637a 0xea9d34 0x56949f;
    "github-light" "GitHub Light" 0xffffff 0xf6f8fa 0xd0d7de 0xffffff 0x1f2328 0x656d76 0x818b98 0x0969da 0xcf222e 0x9a6700 0x1a7f37;
    "flexoki-light" "Flexoki Light" 0xfffcf0 0xf2f0e5 0xe6e4d9 0xfffef8 0x100f0f 0x6f6e69 0x878580 0x24837b 0xaf3029 0xad8301 0x66800b;
}

impl ThemeSeed {
    pub fn is_light(&self) -> bool {
        // Relative luminance of the background, coarse but enough to sort.
        let (r, g, b) = (self.bg >> 16 & 0xff, self.bg >> 8 & 0xff, self.bg & 0xff);
        (r * 299 + g * 587 + b * 114) / 1000 > 128
    }
}

/// Zeron's accent presets: (id, name, on dark, on light).
pub const ACCENTS: [(&str, &str, u32, u32); 7] = [
    ("zeron", "Violet", 0x8b7cf6, 0x5b43e8),
    ("orange", "Orange", 0xfb923c, 0xc2410c),
    ("amber", "Amber", 0xfbbf24, 0xa16207),
    ("green", "Green", 0x4ade80, 0x15803d),
    ("cyan", "Cyan", 0x22d3ee, 0x0e7490),
    ("blue", "Blue", 0x60a5fa, 0x2563eb),
    ("pink", "Pink", 0xf472b6, 0xbe185d),
];

pub fn find(id: &str) -> &'static ThemeSeed {
    THEMES.iter().find(|t| t.id == id).unwrap_or(&THEMES[0])
}

/// The theme for one side of the scheme. An id from the old, longer list
/// (or one of the wrong brightness) falls back to IronClaw's own, so old
/// preference files migrate on their own.
pub fn find_for(id: &str, light: bool) -> &'static ThemeSeed {
    THEMES
        .iter()
        .find(|t| t.id == id && t.is_light() == light)
        .unwrap_or_else(|| {
            find(if light {
                DEFAULT_LIGHT_THEME
            } else {
                DEFAULT_THEME
            })
        })
}
