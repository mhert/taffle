// The options panel: every flag a conversion takes on the command line, as something to type in or
// switch on. What is typed goes into the bridge as it is typed, and what is shown is read back out
// of the bridge on its revision — so a queued book opened again fills these fields with what was
// typed into them, and adding a book leaves them at their defaults.
//
// The field labels are the words the bridge names the fields by when it refuses one, so a refusal
// always names something that is on the screen. They stand over their fields rather than beside
// them: this is a column a third of the window wide, and a label as long as "Add pause each
// chapter" beside a path would leave neither of them readable.
import QtQuick
import QtQuick.Controls
import QtQuick.Dialogs
import QtQuick.Layouts
import Taffle

ColumnLayout {
    id: panel

    // The bridge every field is read off and typed into.
    required property TaffleApp app

    // What is wrong and what is merely worth saying. The palette names neither, so both are named
    // here — the refusal in the red nothing else in the window is, the warning in an amber that
    // says a conversion will still run.
    readonly property color errorColor: "#c62828"
    readonly property color warningColor: "#b26a00"

    // A field is read together with the label over it, so what separates two of them has to be
    // wider than what joins one of them: the gap over every label but the first is the wider one.
    readonly property int groupSpacing: 8

    spacing: 4

    FileDialog {
        id: outputDialog
        title: qsTr("Convert to")
        fileMode: FileDialog.SaveFile
        defaultSuffix: "taf"
        nameFilters: [qsTr("Tonie audio files") + " (*.taf)"]
        // What the dialog hands over is a "file://" URL and the bridge takes a plain filesystem
        // path. The URL is unescaped and the leading slash only a path without a drive letter keeps
        // is dropped, so "file:///tmp/a.taf" is the path "/tmp/a.taf" and "file:///C:/a.taf" is the
        // path "C:/a.taf".
        onAccepted: {
            const path = decodeURIComponent(outputDialog.selectedFile.toString().replace(/^file:\/\//, ""))
            panel.app.setOutput(/^\/[A-Za-z]:/.test(path) ? path.substring(1) : path)
        }
    }

    Label {
        text: qsTr("Output")
    }
    RowLayout {
        Layout.fillWidth: true
        spacing: 4

        TextField {
            Layout.fillWidth: true
            // An empty field is the name the conversion derives from the first input, and this is
            // that name: what will be written, rather than a guess at what will be.
            placeholderText: {
                panel.app.revision;
                panel.app.derivedOutput()
            }
            text: {
                panel.app.revision;
                panel.app.outputText()
            }
            onTextEdited: panel.app.setOutput(text)
        }
        Button {
            text: qsTr("Browse…")
            onClicked: outputDialog.open()
        }
    }

    Label {
        Layout.topMargin: panel.groupSpacing
        text: qsTr("Chapters")
    }
    TextField {
        Layout.fillWidth: true
        // The example is the grammar itself, which no translation may move.
        placeholderText: "0:00,12:34,1:02:10.5"
        text: {
            panel.app.revision;
            panel.app.chaptersText()
        }
        onTextEdited: panel.app.setChapters(text)
    }

    Label {
        Layout.topMargin: panel.groupSpacing
        text: qsTr("Skip leading")
    }
    TextField {
        Layout.fillWidth: true
        placeholderText: qsTr("seconds")
        text: {
            panel.app.revision;
            panel.app.skipLeadingText()
        }
        onTextEdited: panel.app.setSkipLeading(text)
    }

    Label {
        Layout.topMargin: panel.groupSpacing
        text: qsTr("Add pause leading")
    }
    TextField {
        Layout.fillWidth: true
        placeholderText: qsTr("seconds")
        text: {
            panel.app.revision;
            panel.app.addPauseLeadingText()
        }
        onTextEdited: panel.app.setAddPauseLeading(text)
    }

    Label {
        Layout.topMargin: panel.groupSpacing
        text: qsTr("Add pause each chapter")
    }
    TextField {
        Layout.fillWidth: true
        placeholderText: qsTr("seconds")
        text: {
            panel.app.revision;
            panel.app.addPauseEachText()
        }
        onTextEdited: panel.app.setAddPauseEach(text)
    }

    CheckBox {
        Layout.topMargin: panel.groupSpacing
        text: qsTr("Trim leading pause")
        checked: {
            panel.app.revision;
            panel.app.trimLeading()
        }
        onToggled: panel.app.setTrimLeading(checked)
    }
    CheckBox {
        text: qsTr("Trim pause at every chapter")
        checked: {
            panel.app.revision;
            panel.app.trimEachChapter()
        }
        onToggled: panel.app.setTrimEachChapter(checked)
    }
    CheckBox {
        text: qsTr("Extract cover art")
        checked: {
            panel.app.revision;
            panel.app.extractCover()
        }
        onToggled: panel.app.setExtractCover(checked)
    }

    Label {
        Layout.fillWidth: true
        Layout.topMargin: panel.groupSpacing
        wrapMode: Text.Wrap
        // What was typed is no conversion, and this is the sentence that says which part of it.
        visible: panel.app.panelError !== ""
        color: panel.errorColor
        text: panel.app.panelError
    }
    Label {
        Layout.fillWidth: true
        Layout.topMargin: panel.groupSpacing
        wrapMode: Text.Wrap
        // A book that plans more chapters than a box plays converts all the same, so this is a word
        // about it and never a refusal of it.
        visible: panel.app.chapterWarning !== ""
        color: panel.warningColor
        text: panel.app.chapterWarning
    }

    // The fields stand at the top of the column, whatever the window's height leaves under them.
    Item {
        Layout.fillHeight: true
    }
}
