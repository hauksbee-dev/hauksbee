/**
 * Headless Excalidraw diagram generator for the hauksbee docs.
 *
 * Renders every spec in specs/*.json to docs/assets/diagrams/<name>.svg using
 * the Wiley diagram pipeline (adapted from nullhacks2/src/renderer/diagram-*,
 * vendored under wiley/ with provenance headers): spec -> ELK layered layout
 * -> Excalidraw element skeletons -> exportToSvg, with the pipeline's own
 * quality evaluation as a hard gate.
 *
 * Run:  bun render.ts            (all specs)
 *       bun render.ts authority-chain   (one spec by basename)
 */
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, basename } from "node:path";

GlobalRegistrator.register();

// happy-dom ships no canvas 2D context, and Excalidraw both feature-probes it
// at import ("filter" in ctx) and measures label text through it. The width
// model below (0.5em per glyph for Excalifont at export sizes) only has to
// agree with the layout's own estimates well enough that boxes fit their text.
const measure = (text: string, font: string): number => {
  const size = Number(/(\d+(?:\.\d+)?)px/.exec(font)?.[1] ?? 16);
  let width = 0;
  for (const ch of text) width += ch.codePointAt(0)! >= 0x1f000 ? size * 1.2 : size * 0.5;
  return width;
};
(globalThis as never as { HTMLCanvasElement: { prototype: { getContext: unknown } } })
  .HTMLCanvasElement.prototype.getContext = function getContext() {
    const ctx = {
      canvas: this,
      filter: "none",
      font: "16px sans-serif",
      measureText: (text: string) => ({
        width: measure(text, ctx.font),
        actualBoundingBoxAscent: 0,
        actualBoundingBoxDescent: 0,
      }),
      save() {}, restore() {}, scale() {}, translate() {}, rotate() {},
      beginPath() {}, closePath() {}, moveTo() {}, lineTo() {}, stroke() {},
      fill() {}, fillRect() {}, clearRect() {}, fillText() {}, strokeText() {},
      setTransform() {}, getTransform() { return { a: 1, b: 0, c: 0, d: 1, e: 0, f: 0 }; },
      createLinearGradient() { return { addColorStop() {} }; },
      getImageData() { return { data: new Uint8ClampedArray(4) }; },
      putImageData() {}, drawImage() {},
    };
    return ctx;
  };

// The export path registers Excalidraw's fonts through FontFace/document.fonts,
// which happy-dom does not model. The shim accepts and "loads" everything; the
// real font data is inlined into the SVG by exportToSvg from the package's own
// woff2 assets, so nothing visual depends on these stubs.
class FontFaceShim {
  family: string;
  source: string | ArrayBuffer;
  status = "loaded";
  style = "normal";
  weight = "normal";
  stretch = "normal";
  display = "auto";
  unicodeRange = "U+0-10FFFF";
  constructor(
    family: string,
    source: string | ArrayBuffer,
    descriptors?: Record<string, string>,
  ) {
    this.family = family;
    this.source = source;
    Object.assign(this, descriptors ?? {});
  }
  load() { return Promise.resolve(this); }
}
(globalThis as never as { FontFace: unknown }).FontFace ??= FontFaceShim;
const doc = globalThis.document as never as { fonts?: unknown };
if (!doc.fonts) {
  const faces = new Set<unknown>();
  doc.fonts = {
    add: (face: unknown) => faces.add(face),
    delete: (face: unknown) => faces.delete(face),
    check: () => true,
    load: () => Promise.resolve([]),
    ready: Promise.resolve(),
    forEach: (fn: (face: unknown) => void) => faces.forEach(fn),
    [Symbol.iterator]: () => faces[Symbol.iterator](),
  };
}

const here = import.meta.dir;
const OUT_DIR = join(here, "..", "..", "docs", "assets", "diagrams");
const SPEC_DIR = join(here, "specs");

const { planDiagramLayout } = await import("./wiley/diagram-layout");
const { evaluateDiagramPlan, evaluateConvertedScene, mergeQualityReports } = await import(
  "./wiley/diagram-quality"
);
const { convertToExcalidrawElements, exportToSvg } = await import("@excalidraw/excalidraw");

type SpecFile = { name?: string; background?: string; padding?: number } & Record<string, unknown>;

const only = process.argv[2];
const specs = readdirSync(SPEC_DIR)
  .filter((f) => f.endsWith(".json"))
  .filter((f) => !only || basename(f, ".json") === only);
if (specs.length === 0) {
  console.error(only ? `no spec named ${only} in ${SPEC_DIR}` : `no specs in ${SPEC_DIR}`);
  process.exit(1);
}

let failed = false;
for (const file of specs) {
  const name = basename(file, ".json");
  const spec = JSON.parse(readFileSync(join(SPEC_DIR, file), "utf8")) as SpecFile;
  const { background, padding, ...params } = spec;
  try {
    const plan = await planDiagramLayout(params as never, { x: 0, y: 0 }, `docs-${name}`);
    const converted = convertToExcalidrawElements(plan.skeletons as never);
    const quality = mergeQualityReports(
      evaluateDiagramPlan(plan),
      evaluateConvertedScene(converted as never, plan),
    );
    const defects = Object.entries(quality)
      .filter(([, findings]) => Array.isArray(findings) && findings.length > 0)
      .map(([kind, findings]) => `${kind}: ${(findings as string[]).join(", ")}`);
    if (defects.length > 0) {
      throw new Error(`quality gate: ${defects.join("; ")}`);
    }
    const svg = await exportToSvg({
      elements: converted as never,
      appState: {
        exportBackground: Boolean(background),
        viewBackgroundColor: background ?? "#ffffff",
        exportEmbedScene: false,
      } as never,
      files: null,
      exportPadding: padding ?? 24,
    });
    const out = join(OUT_DIR, `${name}.svg`);
    writeFileSync(out, svg.outerHTML);
    console.log(`${name}.svg  (${plan.nodeCount} nodes, ${plan.edgeCount} edges)`);
  } catch (error) {
    failed = true;
    console.error(`${name}: ${error instanceof Error ? error.message : String(error)}`);
  }
}
process.exit(failed ? 1 : 0);
