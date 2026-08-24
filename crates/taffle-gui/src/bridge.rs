//! The one bridge between QML and Rust: every `QObject` the chrome talks to.

// The bridge macro generates FFI declarations the compiler calls unsafe; the exceptions are
// scoped to the bridge module and each carries its SAFETY reasoning where it stands.
#![allow(unsafe_code)]
// C++ owns every object declared below, so the macro hands each one over boxed. Clippy reads
// that as a needless box while the state behind it is still smaller than a pointerful of
// fields, and it is not ours to change. Both attributes have to be file-scoped: cxx_qt::bridge
// rejects any outer attribute on its module.
#![allow(clippy::unnecessary_box_returns)]

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        /// From cxx-qt-lib.
        type QString = cxx_qt_lib::QString;
    }

    unsafe extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(i32, revision)]
        #[qproperty(bool, smoke_mode, cxx_name = "smokeMode")]
        /// The QML-facing application object.
        type TaffleApp = super::TaffleAppRust;
    }
}

/// The state behind [`qobject::TaffleApp`]: nothing yet — the queue and panel arrive with the
/// dialog they serve.
pub struct TaffleAppRust {
    /// Bumped whenever anything a delegate reads may have changed; QML re-reads on the bump.
    revision: i32,
    /// True when the process was started with `--smoke`: the window reads it and runs the drill.
    smoke_mode: bool,
}

impl Default for TaffleAppRust {
    fn default() -> Self {
        Self {
            revision: 0,
            // QML instantiates this object, so a constructor argument cannot carry the flag;
            // it arrives through the process-wide parsed CLI instead.
            smoke_mode: crate::cli().smoke,
        }
    }
}
