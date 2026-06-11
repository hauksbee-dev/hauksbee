import { useState, useCallback } from 'react'
import { useSimulation } from './hooks/useSimulation'
import { BoardViewer } from './components/BoardViewer'
import { ControlBar } from './components/ControlBar'
import { FootprintPanel } from './components/FootprintPanel'
import { NetPanel } from './components/NetPanel'

interface FootprintInfo {
  ref: string
  value: string
  lib_id: string
  x: number
  y: number
}

const BOARDS = [
  { id: 'pic_programmer', label: 'PIC Programmer', file: '/boards/pic_programmer.kicad_pcb' },
  { id: 'microwave', label: 'Microwave Module', file: '/boards/microwave.kicad_pcb' },
  { id: 'stickhub', label: 'StickHub', file: '/boards/stickhub.kicad_pcb' },
]

const WS_URL = `ws://${window.location.hostname}:3002/ws`

export default function App() {
  const [selectedBoard, setSelectedBoard] = useState('pic_programmer')
  const [selectedNet, setSelectedNet] = useState<string | null>(null)
  const [selectedFp, setSelectedFp] = useState<FootprintInfo | null>(null)

  const { connected, frame, send } = useSimulation(WS_URL)

  const board = BOARDS.find(b => b.id === selectedBoard)!

  const handlePlay = useCallback(() => send({ type: 'Play' }), [send])
  const handlePause = useCallback(() => send({ type: 'Pause' }), [send])

  const handleFootprintClick = useCallback((info: FootprintInfo) => {
    setSelectedFp(info)
  }, [])

  const handleBoardChange = useCallback((id: string) => {
    setSelectedBoard(id)
    setSelectedNet(null)
    setSelectedFp(null)
  }, [])

  return (
    <div className="flex flex-col h-screen" style={{ background: '#020617' }}>
      <ControlBar
        connected={connected}
        frame={frame}
        onPlay={handlePlay}
        onPause={handlePause}
        boardName={board.file}
        boards={BOARDS}
        selectedBoard={selectedBoard}
        onBoardChange={handleBoardChange}
        send={send}
      />

      <div className="flex flex-1 overflow-hidden">
        {/* Board canvas area */}
        <div className="flex-1 relative min-w-0">
          <BoardViewer
            boardFile={board.file}
            frame={frame}
            selectedNet={selectedNet}
            onFootprintClick={handleFootprintClick}
          />

          {/* Floating footprint panel */}
          {selectedFp && (
            <div className="absolute top-3 left-3 z-10">
              <FootprintPanel info={selectedFp} onClose={() => setSelectedFp(null)} />
            </div>
          )}
        </div>

        {/* Right sidebar */}
        <div
          className="flex flex-col gap-2 p-2 overflow-y-auto shrink-0"
          style={{
            width: 260,
            borderLeft: '1px solid #1e293b',
            background: '#0a0f1e',
          }}
        >
          <NetPanel frame={frame} selectedNet={selectedNet} onSelectNet={setSelectedNet} />

          {/* Overlay hints */}
          <div
            className="p-2.5 rounded-lg text-[10px]"
            style={{ background: '#0f172a', border: '1px solid #1e293b' }}
          >
            <div className="font-bold tracking-wider mb-1.5" style={{ color: '#475569' }}>OVERLAY</div>
            <div className="flex flex-col gap-1" style={{ color: '#334155' }}>
              <span>• Hover track = voltage probe</span>
              <span>• Click net panel = highlight + particle flow</span>
              <span>• Click footprint = side info</span>
              <span>• Scroll = zoom · drag = pan</span>
            </div>
          </div>

          {/* Sim data summary */}
          {frame && (
            <div
              className="p-2.5 rounded-lg text-[10px]"
              style={{ background: '#0f172a', border: '1px solid #1e293b' }}
            >
              <div className="font-bold tracking-wider mb-1.5" style={{ color: '#475569' }}>SIM FRAME</div>
              <div className="flex flex-col gap-0.5" style={{ color: '#64748b' }}>
                <span>t = <span style={{ color: '#94a3b8' }}>{frame.time_ms.toFixed(3)} ms</span></span>
                <span>step = <span style={{ color: '#94a3b8' }}>{frame.timestep}</span></span>
                <span>nets = <span style={{ color: '#94a3b8' }}>{Object.keys(frame.net_voltages).length}</span></span>
                <span>components = <span style={{ color: '#94a3b8' }}>{Object.keys(frame.component_states).length}</span></span>
                <span>particles = <span style={{ color: '#94a3b8' }}>{Object.values(frame.signal_particles ?? {}).flat().length}</span></span>
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
