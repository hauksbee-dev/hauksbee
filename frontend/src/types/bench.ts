// What a co-sim run looked like from the bench: the wire shapes the engine
// sends in `cosim.bench` (crates/hauksbee-ir/src/observations.rs). Field names
// and enum ids are the engine's verbatim; the words for each id come from the
// shared label table (lib/labels.ts), never from here.

/** What one MCU pin did over the run. */
export type PinState = 'pin_static_high' | 'pin_static_low' | 'pin_toggling' | 'pin_input'

/** Where the core ended the run. */
export type CoreState = 'core_running' | 'core_parked' | 'core_trapped' | 'core_faulted' | 'core_unknown'

/** Which bus a transaction crossed. */
export type BusKind = 'bus_i2c' | 'bus_spi' | 'bus_uart'

/** What kind of moment a timeline marker is. */
export type EventKind =
  | 'event_reset' | 'event_watchdog_reset' | 'event_stall'
  | 'event_first_uart_byte' | 'event_first_edge' | 'event_fault'

/** One level change of a pin: seconds of simulated time and the new level. */
export interface EdgeSample {
  t_s: number
  high: boolean
}

/** One row of the bench view: a pin the board wires to a net. */
export interface PinRow {
  /** The pin as the firmware names it (`PC13`, `PB5`). */
  pin: string
  /** The board net it drives, when it is wired to one. */
  net?: string | null
  state: PinState
  edges: number
  freq_hz?: number | null
  /** Fraction of the observed time spent high, 0..1. */
  duty?: number | null
  first_edge_s?: number | null
  final_high?: boolean | null
  /** The row in words: `toggling 5.0 Hz, 50% duty`. */
  summary: string
  /** The first few hundred edges, for the lanes. */
  edge_samples?: EdgeSample[]
  /** True when `edge_samples` is shorter than `edges`. */
  samples_truncated?: boolean
}

/** One serial port's output. */
export interface UartRow {
  port: string
  /** The board net the TX pin drives, when the binder knows it. */
  net?: string | null
  /** The configured baud rate; null until the peripheral snapshot decodes it. */
  baud?: number | null
  baud_note?: string | null
  bytes: number
  printable_ratio: number
  first_line?: string | null
  first_byte_s?: number | null
}

/** Where a parked core sits. */
export interface ParkSite {
  pc: number
  symbol?: string | null
  pc_samples: number
  distinct_pcs: number
  quiet_since_s?: number | null
}

/** The bench view of one MCU. */
export interface McuBench {
  mcu_ref: string
  backend: string
  core: CoreState
  /** One line: `running`, `parked at Error_Handler`, `restarted 2 times`. */
  headline: string
  park?: ParkSite | null
  resets: number
  first_activity_s?: number | null
  pins: PinRow[]
  uarts?: UartRow[]
}

/** One decoded bus transaction, as a protocol decoder shows it. */
export interface BusTransaction {
  t_s: number
  bus: BusKind
  device: string
  addr?: number | null
  read: boolean
  bytes: number[]
  reply?: number[]
  /** I2C: whether the address byte was acknowledged. SPI: always true. */
  acked: boolean
  /** The transaction in words: `LM75 0x48 write [0x00] ACK`. */
  summary: string
}

/** A dashed marker on the timeline. */
export interface BenchEvent {
  t_s: number
  kind: EventKind
  mcu_ref: string
  /** One line: `reset line pulsed`, `first byte: hello`. */
  detail: string
}

/** The whole bench view of a run. */
export interface BenchReport {
  /** Simulated seconds the run covered. */
  duration_s: number
  mcus: McuBench[]
  bus_log?: BusTransaction[]
  events?: BenchEvent[]
  /** True when the recorder hit its memory cap: counts stay exact, the
   *  waveform and log are cut short. */
  truncated?: boolean
}
