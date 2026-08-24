// One book in the queue: what it is called, what it holds, and where it is — waiting, converting,
// or what it came to. Every line reads the bridge's revision before it reads its own text, so a
// bump re-evaluates it: the queue is a count of rows rather than a model that reports its changes,
// which is what a list of dozens of rows at most is worth.
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import Taffle

ItemDelegate {
    id: row

    // The bridge every line is read off, and where in the queue this row sits: the view hands the
    // number over, and the bridge answers for it.
    required property TaffleApp app
    required property int index

    readonly property string stateName: {
        row.app.revision;
        row.app.bookStateName(row.index)
    }
    readonly property string result: {
        row.app.revision;
        row.app.bookResult(row.index)
    }
    readonly property real fraction: {
        row.app.revision;
        row.app.bookProgress(row.index)
    }
    // A book that has not run is a book to edit again; one that has is a result to read. The queue
    // does not move under a running batch either, so neither does anything on a row.
    readonly property bool waiting: row.stateName === "ready" && !row.app.converting

    // What a book came to, in the three colours it can come to it in. The palette names no colour
    // for a conversion that went wrong, so these are named here: converted, did not convert, and
    // stopped because somebody said so.
    readonly property color convertedColor: "#2e7d32"
    readonly property color failedColor: "#c62828"
    readonly property color stoppedColor: "#b26a00"

    // A row that cannot be opened does not light up under the pointer as though it could.
    hoverEnabled: row.waiting
    onClicked: if (row.waiting)
        row.app.reopenRow(row.index)

    contentItem: ColumnLayout {
        spacing: 2

        RowLayout {
            Layout.fillWidth: true
            spacing: 4

            Label {
                Layout.fillWidth: true
                elide: Text.ElideRight
                font.bold: true
                text: {
                    row.app.revision;
                    row.app.bookTitle(row.index)
                }
            }
            ToolButton {
                // A glyph rather than a word, and the same one in every language.
                text: "✕"
                visible: row.waiting
                onClicked: row.app.removeRow(row.index)
            }
        }

        Label {
            Layout.fillWidth: true
            elide: Text.ElideRight
            // What the book holds is read after what it is called, and stands behind the title
            // rather than beside it.
            opacity: 0.7
            text: {
                row.app.revision;
                row.app.bookMeta(row.index)
            }
        }

        ProgressBar {
            Layout.fillWidth: true
            // Only a book that is running has anything to count: a book that has converted counts
            // nothing at all, and says what it wrote instead.
            visible: row.stateName === "converting"
            // A book that states no length has nothing to count against, and shows a stripe rather
            // than a percent nobody could stand behind.
            indeterminate: row.fraction < 0
            value: row.fraction
        }

        Label {
            Layout.fillWidth: true
            wrapMode: Text.Wrap
            visible: row.result !== ""
            text: row.result
            color: {
                switch (row.stateName) {
                case "done":
                    return row.convertedColor;
                case "failed":
                    return row.failedColor;
                case "cancelled":
                    return row.stoppedColor;
                // A running book says how much audio is in, which is no verdict on anything.
                default:
                    return palette.text;
                }
            }
        }
    }
}
