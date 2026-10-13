//! The backends this binary was built with (cargo features).

use harness_core::BackendDesc;

pub fn all() -> &'static [BackendDesc] {
    &[
        #[cfg(feature = "ffmpeg")]
        backend_ffmpeg::DESC,
        #[cfg(feature = "libav")]
        backend_ffmpeg::libav::DESC,
        #[cfg(feature = "filmcraft")]
        backend_filmcraft::DESC,
        #[cfg(feature = "own")]
        backend_own::DESC,
    ]
}

pub fn find(name: &str) -> Option<BackendDesc> {
    all().iter().copied().find(|b| b.name == name)
}
