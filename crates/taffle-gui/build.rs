//! Generates and compiles the cxx-qt bridge: C++ for `src/bridge.rs`, the QML module
//! resources, and the Qt link flags. Qt is located via `qmake6`/`qmake` on `PATH`.

use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(QmlModule::new("Taffle").qml_files([
        "qml/Main.qml",
        "qml/BookRow.qml",
        "qml/OptionsPanel.qml",
    ]))
    .files(["src/bridge.rs"])
    .build();
}
