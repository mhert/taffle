// The one window: the queue of books over the book being edited, the options panel beside them,
// and what a batch is started and stopped with underneath. Nothing is decided here — every control
// calls into the TaffleApp bridge, and every row reads back off it.
import QtQuick
import QtQuick.Controls
import QtQuick.Dialogs
import QtQuick.Layouts
import Taffle

ApplicationWindow {
    id: window
    title: qsTr("Taffle")
    width: 900
    height: 560
    // The queue, the book being edited and the options are all shown at once, and this is the size
    // at which each of them still holds a path somebody can read; narrower than this, all three are
    // ellipsis.
    minimumWidth: 900
    minimumHeight: 560
    visible: true

    // The bridge. Held as a property of the window rather than by id alone, so that a delegate
    // taking it as a property of its own can be handed it by name without naming itself.
    readonly property TaffleApp app: TaffleApp {}

    // An empty window instantiates none of what a batch shows — no row, no bar, no line saying
    // what a book came to — so a smoke run of one would boot half the chrome and call it proof.
    // The drill puts a finished batch behind the window before the first frame is laid out, so
    // the run that quits on that frame has drawn the whole of it. The queue is then left at its
    // end: a view creates only the rows it is showing, the drill queues one book for every way a
    // batch can leave one, and three of those rows stand taller than the third of the window the
    // queue keeps — so the last of them, the book that was stopped, is drawn only from there.
    Component.onCompleted: if (window.app.smokeMode) {
        window.app.smokeDrill()
        queue.positionViewAtEnd()
    }

    // The smoke run leaves on the window's own completion signal rather than after a wall-clock
    // interval, which would only ever be a guess at how slowly the slowest machine boots.
    // afterAnimating is emitted on the GUI thread (the other scene graph signals are not) every
    // time the render loop is about to draw a frame, so the first one is the observable proof
    // that the whole tree was instantiated and the window came up.
    onAfterAnimating: if (window.app.smokeMode) Qt.exit(0)

    // What a file dialog and a drop hand over is a URL, and every bridge call takes a plain
    // filesystem path; QML has no call of its own that turns the one into the other. This is the
    // only place in the window that does it, so the dialogs and the panel beside them all hand
    // their URLs here. Only a "file://" URL names something this machine can open, so anything
    // else — a browser's http:// drop, a trash:// entry — has no path at all and comes back empty
    // for the caller to leave out. What is left is unescaped: a URL with no host is its own path
    // ("file:///tmp/a.mp3" is "/tmp/a.mp3"), a URL with a host keeps it as a UNC path
    // ("file://box/share/a.mp3" is "//box/share/a.mp3"), and the leading slash only a path without
    // a drive letter keeps is dropped ("file:///C:/a.mp3" is "C:/a.mp3").
    function localPath(url) {
        const text = url.toString()
        if (!text.startsWith("file://"))
            return ""
        // Whether a third slash follows the two is what says the URL names no host, and the
        // unescaping comes after that reading so an escaped slash in a name cannot pose as one.
        const rest = text.substring("file://".length)
        const path = decodeURIComponent(rest.startsWith("/") ? rest : "//" + rest)
        return /^\/[A-Za-z]:/.test(path) ? path.substring(1) : path
    }

    // What addFiles reads: one plain path per line, in the order they were handed over. A URL that
    // names no local file is left out rather than passed on as if it were a path, so a drop that
    // carries nothing openable adds nothing.
    function localPaths(urls) {
        let lines = []
        for (let at = 0; at < urls.length; ++at) {
            const path = window.localPath(urls[at])
            if (path !== "")
                lines.push(path)
        }
        return lines.join("\n")
    }

    FileDialog {
        id: addDialog
        title: qsTr("Add audio files")
        fileMode: FileDialog.OpenFiles
        nameFilters: [qsTr("Audio files") + " (*.m4b *.m4a *.mp3 *.opus *.ogg *.flac *.wav)"]
        onAccepted: window.app.addFiles(window.localPaths(addDialog.selectedFiles))
    }

    ColumnLayout {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 12

        RowLayout {
            Layout.fillWidth: true
            Layout.fillHeight: true
            spacing: 12

            ColumnLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                spacing: 6

                Frame {
                    id: queueFrame
                    // A queue nobody has put a book in is not a queue, and the book being edited
                    // has the whole column to itself.
                    visible: window.app.bookCount > 0
                    Layout.fillWidth: true
                    // The queue grows with what is in it and stops at a third of the window: the
                    // book being edited keeps the rest of the column, however long the batch gets.
                    Layout.preferredHeight: Math.min(queue.contentHeight + queueFrame.topPadding
                                                     + queueFrame.bottomPadding, window.height / 3)

                    ListView {
                        id: queue
                        anchors.fill: parent
                        clip: true
                        spacing: 4
                        // The queue is counted rather than modelled: what a row shows is read off
                        // the bridge by its number, and the bridge's revision is what re-reads it.
                        model: window.app.bookCount
                        delegate: BookRow {
                            app: window.app
                            width: queue.width
                        }

                        ScrollBar.vertical: ScrollBar {}
                    }
                }

                ColumnLayout {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    spacing: 6
                    // A running batch converts the queue as it stood when it was started, so
                    // nothing about the book being edited is touched while one runs.
                    enabled: !window.app.converting

                    Label {
                        text: qsTr("New audiobook")
                        font.bold: true
                    }

                    Frame {
                        Layout.fillWidth: true
                        Layout.fillHeight: true

                        // Files dropped in from outside. It lies under the list so that a row's own
                        // target is what an internal drag lands on; the keys tell the two kinds of
                        // drag apart in any case, and neither ever sees the other's.
                        DropArea {
                            anchors.fill: parent
                            keys: ["text/uri-list"]
                            onDropped: drop => {
                                if (drop.hasUrls)
                                    window.app.addFiles(window.localPaths(drop.urls))
                            }
                        }

                        ListView {
                            id: fileList
                            anchors.fill: parent
                            clip: true
                            model: window.app.fileCount

                            ScrollBar.vertical: ScrollBar {}

                            // The inputs play in the order they are listed and each of them begins
                            // a chapter, so dragging a row is editing the book rather than tidying
                            // a view. The move commits on the drop and not while the row is held:
                            // the rows are numbers that re-read on a revision bump, and reordering
                            // them mid-gesture would rebind the very row being dragged.
                            delegate: Item {
                                id: row
                                width: fileList.width
                                height: content.height
                                property int visualIndex: index

                                Rectangle {
                                    id: content
                                    width: row.width
                                    height: label.implicitHeight + 8
                                    color: dropTarget.containsDrag ? palette.midlight : (row.ListView.isCurrentItem ? palette.highlight : "transparent")

                                    Label {
                                        id: label
                                        anchors.left: parent.left
                                        anchors.right: parent.right
                                        anchors.leftMargin: 6
                                        anchors.rightMargin: 6
                                        anchors.verticalCenter: parent.verticalCenter
                                        elide: Text.ElideMiddle
                                        color: row.ListView.isCurrentItem ? palette.highlightedText : palette.text
                                        text: {
                                            window.app.revision;
                                            window.app.fileAt(row.visualIndex)
                                        }
                                    }

                                    Drag.active: dragArea.drag.active
                                    Drag.source: row
                                    Drag.keys: ["taffle-file-row"]
                                    Drag.hotSpot.y: content.height / 2
                                    states: State {
                                        when: dragArea.drag.active
                                        ParentChange {
                                            target: content
                                            parent: fileList
                                        }
                                    }

                                    MouseArea {
                                        id: dragArea
                                        anchors.fill: parent
                                        drag.target: content
                                        drag.axis: Drag.YAxis
                                        onReleased: content.Drag.drop()
                                        onClicked: fileList.currentIndex = row.visualIndex
                                    }
                                }

                                DropArea {
                                    id: dropTarget
                                    anchors.fill: parent
                                    keys: ["taffle-file-row"]
                                    // The move takes the selection with it: the bridge answers
                                    // with where the picked row ended up, so Remove stays aimed at
                                    // the file it was aimed at before the drag.
                                    onDropped: drop => {
                                        fileList.currentIndex = window.app.moveFile(drop.source.visualIndex, row.visualIndex, fileList.currentIndex)
                                    }
                                }
                            }
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 8

                        Button {
                            text: qsTr("Add files…")
                            onClicked: addDialog.open()
                        }
                        Button {
                            text: qsTr("Remove")
                            // An empty list has no current row, and no row is nothing to remove.
                            enabled: fileList.currentIndex >= 0
                            onClicked: window.app.removeFile(fileList.currentIndex)
                        }
                        Item {
                            Layout.fillWidth: true
                        }
                    }

                    Button {
                        Layout.fillWidth: true
                        text: qsTr("＋ Add audiobook to batch")
                        // A book that is no conversion stays where it is and the panel says why.
                        onClicked: window.app.addToBatch()
                    }
                }
            }

            OptionsPanel {
                app: window.app
                onOutputPicked: file => window.app.setOutput(window.localPath(file))
                Layout.fillHeight: true
                // A third of the window, and no more: a layout inside a layout would otherwise be
                // handed every pixel the column beside it does not ask for, and the queue and the
                // file list — which are what show whole paths — need those.
                Layout.fillWidth: false
                Layout.preferredWidth: window.width / 3
                // See the editing area above: the options are the book's, and a running batch
                // converts what it was given.
                enabled: !window.app.converting
            }
        }

        RowLayout {
            Layout.fillWidth: true
            spacing: 8

            Item {
                Layout.fillWidth: true
            }
            Button {
                text: qsTr("Clear")
                // The queue does not move under a running batch, so it is not emptied under one.
                enabled: !window.app.converting
                onClicked: window.app.clearAll()
            }
            Button {
                text: window.app.converting ? qsTr("Cancel") : qsTr("Convert")
                highlighted: true
                onClicked: {
                    // Whether a batch started is what this button and the rows under it then show;
                    // a refusal that has anything to say says it in the panel.
                    if (window.app.converting)
                        window.app.cancel()
                    else
                        window.app.convert()
                }
            }
        }
    }
}
