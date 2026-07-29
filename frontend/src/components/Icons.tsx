// Icon set, vetted geometry from Lucide (https://lucide.dev, ISC/MIT), inlined
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

// StepIcon mirrored: walk back through the retained frames.
export const StepBackIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><polygon points="19 4 9 12 19 20 19 4" fill="currentColor" stroke="none" /><line x1="5" x2="5" y1="5" y2="19" /></>, p)

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

// Lucide `zap`; the fault glyph, filled.
export const BoltIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2" fill="currentColor" stroke="none" />, p)

// Lucide `activity`, a waveform, for the scope-attach affordance.
export const ProbeIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <path d="M22 12h-2.48a2 2 0 0 0-1.93 1.46l-2.35 8.36a.25.25 0 0 1-.48 0L9.24 2.18a.25.25 0 0 0-.48 0l-2.35 8.36A2 2 0 0 1 4.49 12H2" />, p)

// Lucide `circuit-board`; the "load a board" slot marker (also the Board nav
// glyph in the sidebar).
export const BoardTargetIcon = ({ size = 20, ...p }: IconProps) =>
  svg(size, <><rect width="18" height="18" x="3" y="3" rx="2" /><path d="M11 9h4a2 2 0 0 0 2-2V3" /><circle cx="9" cy="9" r="2" /><path d="M7 21v-4a2 2 0 0 1 2-2h4" /><circle cx="15" cy="15" r="2" /></>, p)

// Lucide `list-checks`; the Checks nav glyph.
export const ChecksIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="m3 17 2 2 4-4" /><path d="m3 7 2 2 4-4" /><path d="M13 6h8" /><path d="M13 12h8" /><path d="M13 18h8" /></>, p)

// Lucide `radio`; the Live Sim nav glyph (waves off an antenna).
export const LiveIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M4.9 19.1C1 15.2 1 8.8 4.9 4.9" /><path d="M7.8 16.2c-2.3-2.3-2.3-6.1 0-8.5" /><circle cx="12" cy="12" r="2" /><path d="M16.2 7.8c2.3 2.3 2.3 6.1 0 8.5" /><path d="M19.1 4.9C23 8.8 23 15.2 19.1 19.1" /></>, p)

// Lucide `wrench`; the Environment (deps/doctor) nav glyph.
export const WrenchIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z" />, p)

// Lucide `layers`.
export const LayersIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83z" /><path d="m22 17.65-9.17 4.16a2 2 0 0 1-1.66 0L2 17.65" /><path d="m22 12.65-9.17 4.16a2 2 0 0 1-1.66 0L2 12.65" /></>, p)

// Lucide `maximize`; the fit-to-view control.
export const FitIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M8 3H5a2 2 0 0 0-2 2v3" /><path d="M21 8V5a2 2 0 0 0-2-2h-3" /><path d="M3 16v3a2 2 0 0 0 2 2h3" /><path d="M16 21h3a2 2 0 0 0 2-2v-3" /></>, p)

// Lucide `sun`.
export const SunIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><circle cx="12" cy="12" r="4" /><path d="M12 2v2" /><path d="M12 20v2" /><path d="m4.93 4.93 1.41 1.41" /><path d="m17.66 17.66 1.41 1.41" /><path d="M2 12h2" /><path d="M20 12h2" /><path d="m6.34 17.66-1.41 1.41" /><path d="m19.07 4.93-1.41 1.41" /></>, p)

// Lucide `moon`.
export const MoonIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z" />, p)

// Lucide `eye` / `eye-off`, for layer and trace visibility toggles.
export const EyeIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M2.06 12.35a1 1 0 0 1 0-.7 10.75 10.75 0 0 1 19.88 0 1 1 0 0 1 0 .7 10.75 10.75 0 0 1-19.88 0" /><circle cx="12" cy="12" r="3" /></>, p)

export const EyeOffIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M10.73 5.08A10.43 10.43 0 0 1 12 5c7 0 10 7 10 7a13.16 13.16 0 0 1-1.67 2.68" /><path d="M6.61 6.61A13.53 13.53 0 0 0 2 12s3 7 10 7a9.74 9.74 0 0 0 5.39-1.61" /><path d="M2 2l20 20" /><path d="M9.9 4.24A9.12 9.12 0 0 1 12 4" opacity="0" /><path d="M14.12 14.12a3 3 0 1 1-4.24-4.24" /></>, p)

// Lucide `chevron-down` / `chevron-right`, for collapsible cards.
export const ChevronDownIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <path d="m6 9 6 6 6-6" />, p)

export const ChevronRightIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <path d="m9 18 6-6-6-6" />, p)

// Lucide `cpu`; MCU stat chips.
export const ExpandIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M8 3H5a2 2 0 0 0-2 2v3" /><path d="M21 8V5a2 2 0 0 0-2-2h-3" /><path d="M3 16v3a2 2 0 0 0 2 2h3" /><path d="M16 21h3a2 2 0 0 0 2-2v-3" /></>, p)

export const CollapseIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M8 3v3a2 2 0 0 1-2 2H3" /><path d="M21 8h-3a2 2 0 0 1-2-2V3" /><path d="M3 16h3a2 2 0 0 1 2 2v3" /><path d="M16 21v-3a2 2 0 0 1 2-2h3" /></>, p)

export const WarningIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3" /><path d="M12 9v4" /><path d="M12 17h.01" /></>, p)

export const CpuIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><rect width="16" height="16" x="4" y="4" rx="2" /><rect width="6" height="6" x="9" y="9" /><path d="M15 2v2" /><path d="M15 20v2" /><path d="M2 15h2" /><path d="M2 9h2" /><path d="M20 15h2" /><path d="M20 9h2" /><path d="M9 2v2" /><path d="M9 20v2" /></>, p)

// Lucide `terminal`; the serial console card.
export const TerminalIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><polyline points="4 17 10 11 4 5" /><line x1="12" x2="20" y1="19" y2="19" /></>, p)

// Lucide `sliders-horizontal`; the inputs / solver cards.
export const SlidersIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><line x1="21" x2="14" y1="4" y2="4" /><line x1="10" x2="3" y1="4" y2="4" /><line x1="21" x2="12" y1="12" y2="12" /><line x1="8" x2="3" y1="12" y2="12" /><line x1="21" x2="16" y1="20" y2="20" /><line x1="12" x2="3" y1="20" y2="20" /><line x1="14" x2="14" y1="2" y2="6" /><line x1="8" x2="8" y1="10" y2="14" /><line x1="16" x2="16" y1="18" y2="22" /></>, p)

// Lucide `plug-zap`; the power rails card.
export const PowerIcon = ({ size = 14, ...p }: IconProps) =>
  svg(size, <><path d="M6.3 20.3a2.4 2.4 0 0 0 3.4 0L12 18l-6-6-2.3 2.3a2.4 2.4 0 0 0 0 3.4Z" /><path d="m2 22 3-3" /><path d="M7.5 13.5 10 11" /><path d="M10.5 16.5 13 14" /><path d="m18 3-4 4h6l-4 4" /></>, p)
