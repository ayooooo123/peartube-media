use std::path::PathBuf;
use std::sync::OnceLock;
use sha2::{Digest, Sha256};
use subs_render::FontOptions;

pub fn fonts() -> &'static PathBuf {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY.get_or_init(|| {
        let path = std::env::var_os("SUBTITLE_TEST_FONTS").map(PathBuf::from).unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME or SUBTITLE_TEST_FONTS")).join("projects/peartube-media-corpus/fonts/subs")
        });
        for (name, sha) in [
            ("DejaVuSans.ttf", "7da195a74c55bef988d0d48f9508bd5d849425c1770dba5d7bfc6ce9ed848954"),
            ("DejaVuSans-Bold.ttf", "e6476c1b80502924294eed40894c5b18e06c181444ca953e5334262df9c27724"),
            ("DejaVuSans-Oblique.ttf", "4af75fa16ee6d3ad43e1ecec41862c24954af26a55c6bb1ebb27bd486a50f5f4"),
            ("DejaVuSans-BoldOblique.ttf", "eb436dca0c2594b73d8b603b892e374fdfd8d885d25ffb4f18df4c4c0b49e50f"),
            ("DejaVuSansMono.ttf", "b4a6c3e4faab8773f4ff761d56451646409f29abedd68f05d38c2df667d3c582"),
            ("DejaVuSerif.ttf", "42d1edeb7952f31b1f96d767ed7030b08a39e0c372b0071641518864e2bffb51"),
            ("NotoSansDevanagari-Regular.ttf", "9c7d935139ea6a1e6ad9dbac4f6d27ece1e04bca8123c8888d00a0f9df4724cd"),
        ] {
            let bytes = std::fs::read(path.join(name)).unwrap_or_else(|e| panic!("{name}: {e}; run scripts/fetch-subtitle-fonts.py"));
            assert_eq!(format!("{:x}", Sha256::digest(bytes)), sha, "{name}: wrong test font");
        }
        path
    })
}

pub fn options() -> FontOptions {
    FontOptions { directories: Some(vec![fonts().clone()]), default_family: Some("DejaVu Sans".into()) }
}
