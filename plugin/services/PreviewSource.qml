import QtQuick
import Quickshell.Io

// One selected-file request, separate from the long-lived FFF search process.
Item {
  id: service
  property string path: ""
  property bool active: false
  property var helperCandidates: []
  property var result: ({})
  property bool busy: false
  property int revision: 0
  property int launchedRevision: -1
  property string launchedPath: ""
  property var answer: null
  property int binaryIndex: 0
  property string phase: "idle"
  property bool timedOut: false

  function request() {
    service.revision++
    debounce.stop()
    service.result = ({})
    service.binaryIndex = 0
    if (proc.running) { proc.signal(15); forceKill.restart() }
    service.busy = service.active && service.path.length > 0
    if (service.busy) debounce.restart()
  }
  function fail(message) {
    service.result = ({ message: message, error: true })
    service.busy = false
    service.phase = "idle"
  }
  onPathChanged: request()
  onActiveChanged: request()
  Component.onDestruction: if (proc.running) proc.signal(15)

  Timer {
    id: debounce
    interval: 160
    onTriggered: {
      if (!service.active || !service.path) return
      if (proc.running) { debounce.restart(); return }
      if (service.binaryIndex >= service.helperCandidates.length) {
        service.fail("Preview helper missing — run: oma-cast files build")
        return
      }
      service.launchedPath = service.path
      service.launchedRevision = service.revision
      service.answer = null
      service.timedOut = false
      service.phase = "starting"
      proc.command = [service.helperCandidates[service.binaryIndex], "--preview", service.launchedPath]
      proc.running = true
      deadline.restart()
    }
  }
  Timer {
    id: forceKill
    interval: 500
    onTriggered: if (proc.running) proc.signal(9)
  }
  Timer {
    id: deadline
    interval: 12000
    onTriggered: {
      service.timedOut = true
      if (proc.running) { proc.signal(15); forceKill.restart() }
    }
  }
  Process {
    id: proc
    stdout: SplitParser {
      onRead: function(line) {
        if (line.length > 80000) return
        try {
          var value = JSON.parse(line)
          if (value.path === service.launchedPath) service.answer = value
        } catch (e) {}
      }
    }
    onStarted: { service.phase = "running"; forceKill.stop() }
    onRunningChanged: {
      // QProcess::FailedToStart does not emit exited. Try the next standard
      // helper location; this must not strand the preview in a loading state.
      if (running || service.phase !== "starting") return
      Qt.callLater(function() {
        if (proc.running || service.phase !== "starting") return
        deadline.stop()
        service.phase = "idle"
        if (service.launchedRevision !== service.revision || !service.active) return
        service.binaryIndex++
        debounce.restart()
      })
    }
    onExited: function(code, status) {
      forceKill.stop()
      deadline.stop()
      service.phase = "idle"
      if (!service.active || service.revision !== service.launchedRevision || service.path !== service.launchedPath) return
      service.result = service.timedOut
        ? ({ message: "Preview timed out — try a smaller or local file", error: true })
        : service.answer || ({ message: "Preview helper exited without a valid reply — try: oma-cast files build", error: true })
      service.busy = false
    }
  }
}
