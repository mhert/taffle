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

    Timer {
        id: smokeQuit
        interval: 700
        running: app.smokeMode
        onTriggered: Qt.exit(0)
    }
}
