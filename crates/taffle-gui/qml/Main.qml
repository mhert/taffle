// The one window. While the dialog is being built out, the smoke contract is already honored:
// booted with the smoke flag, the window exercises itself and quits with 0.
import QtQuick
import QtQuick.Controls
import Taffle

ApplicationWindow {
    id: window
    title: qsTr("Taffle")
    width: 900
    height: 560
    visible: true

    TaffleApp { id: app }

    // The smoke run leaves on the window's own completion signal rather than after a wall-clock
    // interval, which would only ever be a guess at how slowly the slowest machine boots.
    // afterAnimating is emitted on the GUI thread (the other scene graph signals are not) every
    // time the render loop is about to draw a frame, so the first one is the observable proof
    // that the whole tree was instantiated and the window came up.
    onAfterAnimating: if (app.smokeMode) Qt.exit(0)
}
