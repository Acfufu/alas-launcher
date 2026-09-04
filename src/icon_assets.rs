//! Runtime tray-icon asset gate (PR-review fallout, 2026-09-05).
//!
//! Two defect classes shipped to the menu bar before this test existed:
//! stroke-only SVGs rasterized to fully-transparent PNGs (missing stroke
//! color), and nothing failing loudly on it. These checks run in
//! `cargo test` on every platform and pin the two assets `tray.rs` actually
//! embeds: template format (pure black + alpha), 64×64 (@2x) size, and an
//! ink-coverage band that fails on both blank and over-filled renders.

#[cfg(test)]
mod tests {
    const ASSETS: [(&[u8], &str, f64, f64); 2] = [
        // (png bytes, label, min ink %, max ink %)
        (
            include_bytes!("../icons/tray-icon.png"),
            "tray-icon.png (实心/运行中)",
            8.0,
            30.0,
        ),
        (
            include_bytes!("../icons/tray-icon-outline.png"),
            "tray-icon-outline.png (描边/已停止)",
            4.0,
            25.0,
        ),
    ];

    #[test]
    fn runtime_icon_assets_are_valid_templates() {
        for (bytes, label, min_ink, max_ink) in ASSETS {
            let img = tauri::image::Image::from_bytes(bytes)
                .unwrap_or_else(|e| panic!("{label}: PNG decode failed: {e}"));
            assert_eq!((img.width(), img.height()), (64, 64), "{label}: bad size");

            let rgba = img.rgba();
            assert_eq!(rgba.len(), 64 * 64 * 4, "{label}: bad buffer");

            let mut ink: usize = 0;
            for px in rgba.as_chunks::<4>().0 {
                let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
                // template images: macOS tints purely from alpha — any color
                // channel with visible alpha would render as a wrong tint.
                if a > 8 {
                    assert_eq!((r, g, b), (0, 0, 0), "{label}: non-black visible pixel");
                    ink += 1;
                }
            }
            let ink_pct = 100.0 * ink as f64 / (64.0 * 64.0);
            assert!(
                (min_ink..=max_ink).contains(&ink_pct),
                "{label}: ink {ink_pct:.1}% outside [{min_ink}, {max_ink}] — blank or corrupted render?"
            );
        }
    }
}
