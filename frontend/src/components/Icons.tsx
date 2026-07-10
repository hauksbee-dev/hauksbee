// Icon set — vetted geometry from Lucide (https://lucide.dev, ISC/MIT), inlined
// rather than pulled as a runtime dependency so the bundle stays small and the
// served UI is self-contained. Every icon is Lucide's 24×24 grid at a 2px
// stroke, drawn on `currentColor` so each call site controls colour. Pass
// `size` to scale.

type IconProps = { size?: number; className?: string; style?: React.CSSProperties }

function svg(size: number, children: React.ReactNode, extra?: IconProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2}
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

// Transport glyphs read better filled at small sizes; they use Lucide's
// vertices with a solid fill.
export const PlayIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <polygon points="6 3 20 12 6 21 6 3" fill="currentColor" stroke="none" />, p)

export const PauseIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><rect x="14" y="4" width="4" height="16" rx="1" fill="currentColor" stroke="none" /><rect x="6" y="4" width="4" height="16" rx="1" fill="currentColor" stroke="none" /></>, p)

// Lucide `skip-forward`.
export const StepIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><polygon points="5 4 15 12 5 20 5 4" fill="currentColor" stroke="none" /><line x1="19" x2="19" y1="5" y2="19" /></>, p)

// Lucide `rotate-ccw`.
export const ResetIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" /><path d="M3 3v5h5" /></>, p)

// Lucide `x`.
export const CloseIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M18 6 6 18" /><path d="m6 6 12 12" /></>, p)

// Lucide `plus`.
export const PlusIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M5 12h14" /><path d="M12 5v14" /></>, p)

// Lucide `check`.
export const CheckIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <path d="M20 6 9 17l-5-5" />, p)

// Lucide `arrow-left`.
export const BackIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="m12 19-7-7 7-7" /><path d="M19 12H5" /></>, p)

// Lucide `zap` — the fault glyph, filled.
export const BoltIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2" fill="currentColor" stroke="none" />, p)

// Lucide `activity` — a waveform, for the scope-attach affordance.
export const ProbeIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <path d="M22 12h-2.48a2 2 0 0 0-1.93 1.46l-2.35 8.36a.25.25 0 0 1-.48 0L9.24 2.18a.25.25 0 0 0-.48 0l-2.35 8.36A2 2 0 0 1 4.49 12H2" />, p)

// Lucide `circuit-board` — the "load a board" slot marker.
export const BoardTargetIcon = ({ size = 20, ...p }: IconProps) =>
  svg(size, <><rect width="18" height="18" x="3" y="3" rx="2" /><path d="M11 9h4a2 2 0 0 0 2-2V3" /><circle cx="9" cy="9" r="2" /><path d="M7 21v-4a2 2 0 0 1 2-2h4" /><circle cx="15" cy="15" r="2" /></>, p)
