import { useCallback, useRef, useState } from 'react'

// Drag-over feedback for a drop target.
//
// interior.dev has no drop-zone component, so this is built from its
// vocabulary rather than vendored: the state machine here is the interesting
// part, and it exists because the naive version is wrong in two ways that show
// up immediately.
//
// First, `dragenter`/`dragleave` fire for every descendant the pointer crosses.
// A drop card with an icon, a heading and a button emits a leave the moment the
// cursor moves from the card onto the heading, so a single-boolean zone flickers
// as the user moves toward the middle of it. The depth counter below is the fix:
// only the leave that balances the outermost enter counts.
//
// Second, and more important: the zone must not claim to ACCEPT something it
// has not looked at. During a drag the browser withholds file names (the
// dataTransfer entries expose `kind` and `type`, never `name`, until the drop),
// so "accepted" is not knowable and a green tick would be a guess. What IS
// knowable is whether the drag carries files at all, which is the one case
// worth a distinct answer: dragging selected text or a link onto the board zone
// gets a refusal instead of an invitation, before the user lets go.

export type DropState =
  /** Nothing being dragged over this target. */
  | 'idle'
  /** Files are over the target. Whether they are readable is the drop's answer. */
  | 'over'
  /** Something is over the target that is definitely not a file. */
  | 'reject'

/** Does this drag carry files? `dataTransfer.types` includes 'Files' for a
 *  file drag in every current browser; the items list is the cross-check. */
function carriesFiles(dt: DataTransfer | null): boolean {
  if (!dt) return false
  if (Array.from(dt.types).includes('Files')) return true
  return Array.from(dt.items ?? []).some(i => i.kind === 'file')
}

export function useDropTarget(onFiles: (files: FileList) => void): {
  state: DropState
  bind: {
    onDragEnter: (e: React.DragEvent) => void
    onDragOver: (e: React.DragEvent) => void
    onDragLeave: (e: React.DragEvent) => void
    onDrop: (e: React.DragEvent) => void
  }
} {
  const [state, setState] = useState<DropState>('idle')
  const depth = useRef(0)

  const reset = useCallback(() => {
    depth.current = 0
    setState('idle')
  }, [])

  const bind = {
    onDragEnter: (e: React.DragEvent) => {
      e.preventDefault()
      depth.current += 1
      setState(carriesFiles(e.dataTransfer) ? 'over' : 'reject')
    },
    onDragOver: (e: React.DragEvent) => {
      e.preventDefault()
      // Say out loud that this is a copy, so the cursor shows the copy badge
      // rather than the browser's default "no" for a target it has not been
      // told about.
      const files = carriesFiles(e.dataTransfer)
      if (e.dataTransfer) e.dataTransfer.dropEffect = files ? 'copy' : 'none'
      // dragover also acts as the keep-alive: a dragenter that was missed
      // (entering through a child that stopped propagation) still resolves.
      if (depth.current === 0) depth.current = 1
      setState(files ? 'over' : 'reject')
    },
    onDragLeave: (e: React.DragEvent) => {
      e.preventDefault()
      depth.current = Math.max(0, depth.current - 1)
      if (depth.current === 0) setState('idle')
    },
    onDrop: (e: React.DragEvent) => {
      e.preventDefault()
      reset()
      const files = e.dataTransfer?.files
      if (files && files.length > 0) onFiles(files)
    },
  }

  return { state, bind }
}
