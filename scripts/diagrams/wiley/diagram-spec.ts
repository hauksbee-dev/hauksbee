// Vendored from the Wiley whiteboard project (nullhacks2 src/renderer/diagram-spec.ts),
// adapted only in import paths. Regenerate docs SVGs with: bun run generate
import type { GraphEdge, LayoutParams } from "./diagram-layout";

const SLUG_MAX_LENGTH = 40;
const HASH_LENGTH = 6;
const HASH_SPACE = 36 ** HASH_LENGTH;

/** FNV-1a, 32 bit. Cheap, stable across runs, and good enough to separate
 * two labels that collide once truncated to the slug length. */
function fnv1a(value: string): number {
  let hash = 0x811c9dc5;
  for (let index = 0; index < value.length; index++) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

/**
 * Readable, deterministic, collision-resistant. The readable half is
 * truncated so ids stay legible in the scene JSON; the hash is taken from
 * the untruncated original so two long labels sharing a prefix never
 * produce the same id.
 */
export function slug(value: string): string {
  const source = String(value ?? "");
  const normalized = source
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
  const readable = normalized.slice(0, SLUG_MAX_LENGTH).replace(/-+$/, "");
  const hash = (fnv1a(source) % HASH_SPACE).toString(36).padStart(HASH_LENGTH, "0");
  return readable ? `${readable}-${hash}` : hash;
}

/**
 * A diagram's stable identity. The seed separates two diagrams drawn from
 * identical parameters; production passes a clock reading, tests pass a
 * fixed number so the whole id is reproducible.
 */
export function deriveDiagramId(params: LayoutParams, seed: number = Date.now()): string {
  const name = params.title?.trim() || params.nodes?.[0]?.label?.trim() || "diagram";
  const suffix = Math.abs(Math.trunc(seed)).toString(36);
  return `wd-${slug(name)}-${suffix}`;
}

/**
 * Ordinal of each edge among the parallel edges sharing its endpoints, so
 * two `a -> b` edges get distinct ids.
 */
export function edgeOrdinals(edges: readonly GraphEdge[]): number[] {
  const seen = new Map<string, number>();
  return edges.map((edge) => {
    const pair = `${edge.from}\u0000${edge.to}`;
    const ordinal = seen.get(pair) ?? 0;
    seen.set(pair, ordinal + 1);
    return ordinal;
  });
}

/** The per-diagram key an edge is addressed by, in ids and in customData. */
export function edgeKey(edge: GraphEdge, ordinal: number): string {
  return `${slug(edge.from)}__${slug(edge.to)}__${ordinal}`;
}

export function nodeElementId(diagramId: string, nodeId: string): string {
  return `${diagramId}-n-${slug(nodeId)}`;
}

export function edgeElementId(diagramId: string, key: string): string {
  return `${diagramId}-e-${key}`;
}

export function edgeLabelElementId(diagramId: string, key: string): string {
  return `${diagramId}-el-${key}`;
}

export function titleElementId(diagramId: string): string {
  return `${diagramId}-title`;
}

export function containerElementId(diagramId: string, containerId: string): string {
  return `${diagramId}-c-${slug(containerId)}`;
}

export function containerLabelElementId(diagramId: string, containerId: string): string {
  return `${diagramId}-cl-${slug(containerId)}`;
}

/** The Excalidraw group every element inside a container shares. */
export function containerGroupId(diagramId: string, containerId: string): string {
  return `${diagramId}-g-${slug(containerId)}`;
}
