// A small line-icon set (1.5px stroke, 16px grid) replacing the emoji glyphs
// that read as placeholder/AI-slop. Consistent instrument aesthetic: geometric,
// monochrome, `currentColor` so each call site controls color. Pass `size` to
// scale; every icon shares the same 16-unit viewBox so they align in a row.

type IconProps = { size?: number; className?: string; style?: React.CSSProperties }

function svg(size: number, children: React.ReactNode, extra?: IconProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={extra?.className}
      style={extra?.style}
      aria-hidden="true"
    >
      {children}
    </svg>
  )
}

export const PlayIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <path d="M4.5 3.2 L12.5 8 L4.5 12.8 Z" fill="currentColor" stroke="none" />, p)

export const PauseIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><rect x="4.5" y="3.5" width="2.3" height="9" rx="0.5" fill="currentColor" stroke="none" /><rect x="9.2" y="3.5" width="2.3" height="9" rx="0.5" fill="currentColor" stroke="none" /></>, p)

// Step-forward: play bar + tick.
export const StepIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M4 3.5 L10 8 L4 12.5 Z" fill="currentColor" stroke="none" /><path d="M12 3.5 V12.5" /></>, p)

// Reset / restart: a circular arrow.
export const ResetIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M12.5 8 A4.5 4.5 0 1 1 10.8 4.5" /><path d="M12.6 2.2 V4.9 H9.9" /></>, p)

export const CloseIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M4 4 L12 12" /><path d="M12 4 L4 12" /></>, p)

export const PlusIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M8 3.5 V12.5" /><path d="M3.5 8 H12.5" /></>, p)

export const CheckIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <path d="M3.5 8.5 L6.5 11.5 L12.5 4.5" />, p)

export const BackIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M10 3.5 L5.5 8 L10 12.5" /><path d="M5.5 8 H12.5" opacity="0.55" /></>, p)

// A lightning bolt for faults (replaces the ⚡ emoji).
export const BoltIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <path d="M8.8 2 L4 9 H7.5 L6.8 14 L11.5 6.6 H8.2 Z" fill="currentColor" stroke="none" />, p)

// A probe tip (oscilloscope probe) — for the scope-attach affordance.
export const ProbeIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M9.5 2.5 L13.5 6.5 L8 12 L4 12 L4 8 Z" /><path d="M2 14 L4.5 11.5" /></>, p)

// A board-input target: concentric square + crosshair (the "load a board" slot).
export const BoardTargetIcon = ({ size = 20, ...p }: IconProps) =>
  svg(size, <><rect x="3" y="3" width="10" height="10" rx="1" /><circle cx="8" cy="8" r="2" /><path d="M8 1 V3 M8 13 V15 M1 8 H3 M13 8 H15" /></>, p)
