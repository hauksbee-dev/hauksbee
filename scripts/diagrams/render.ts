/**
 * Headless draw.io diagram generator for the hauksbee docs.
 *
 * Renders every spec in specs/*.json to a docs/assets/diagrams/<name>.drawio
 * source plus its <name>.svg export, in the same house style as the
 * hand-drawn diagrams beside them (architecture.drawio and friends): rounded
 * dark boxes, the orange/blue/green stroke vocabulary, orthogonal anchored
 * edges, Helvetica 12px.
 *
 * Pipeline: spec -> ELK layered layout (elkjs, node sizes computed from the
 * same font metrics scripts/lint-diagrams.py checks against, so the lint
 * passes by construction) -> mxGraph XML -> SVG via the draw.io desktop app's
 * headless exporter -> scripts/lint-diagrams.py as a hard gate.
 *
 * Run:  bun render.ts                 (all specs)
 *       bun render.ts authority-chain (one spec by basename)
 *
 * Spec shape (specs/*.json):
 *   layout.direction: "DOWN" | "RIGHT"
 *   nodes[]: { id, label ("\n" for line breaks), role: input|stage|check|output }
 *   edges[]: { from, to }
 */
import { readdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { join, basename, resolve } from "node:path";

// elkjs's fake-worker script decides "am I inside a web worker?" by checking
// `typeof document === "undefined" && typeof self !== "undefined"`. Bun
// defines `self` but no `document`, so without this stub the script takes the
// worker branch and never assigns module.exports, and `new ELK()` dies on an
// undefined Worker. An empty object is enough; nothing dereferences it.
(globalThis as { document?: unknown }).document ??= {};
// require, not import: an ESM import would hoist above the stub.
// eslint-disable-next-line @typescript-eslint/no-require-imports
const ELK = require("elkjs/lib/main.js");

// The lint's font model (scripts/lint-diagrams.py): draw.io's default
// Helvetica at 12px. Boxes are sized from the SAME numbers plus margin, so a
// label can never overflow its box by the lint's measure.
const CHAR_W = 6.4;
const LINE_H = 17.0;
const PAD_X = 16.0;
const PAD_Y = 10.0;

// The house palette, read off the existing hand-drawn .drawio files.
const STYLE: Record<string, { stroke: string; fill: string }> = {
  input: { stroke: "#e08a4e", fill: "#0f1729" },
  stage: { stroke: "#3b82f6", fill: "#111c33" },
  check: { stroke: "#22c55e", fill: "#111c33" },
  output: { stroke: "#22c55e", fill: "#111c33" },
};

interface SpecNode {
  id: string;
  label: string;
  role: keyof typeof STYLE;
}
interface Spec {
  layout: { direction: "DOWN" | "RIGHT" };
  nodes: SpecNode[];
  edges: { from: string; to: string }[];
}
interface Placed extends SpecNode {
  x: number;
  y: number;
  w: number;
  h: number;
}

function nodeSize(label: string): { w: number; h: number } {
  const lines = label.split("\n");
  const widest = Math.max(...lines.map((l) => l.length));
  return {
    w: Math.max(120, Math.ceil(widest * CHAR_W + PAD_X + 20)),
    h: Math.max(44, Math.ceil(lines.length * LINE_H + PAD_Y + 14)),
  };
}

const xml = (s: string) =>
  s.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;");

async function layout(spec: Spec): Promise<Placed[]> {
  const elk = new ELK();
  const sized = spec.nodes.map((n) => ({ ...n, ...nodeSize(n.label) }));
  const graph = await elk.layout({
    id: "root",
    layoutOptions: {
      "elk.algorithm": "layered",
      "elk.direction": spec.layout.direction,
      "elk.layered.spacing.nodeNodeBetweenLayers": "56",
      "elk.spacing.nodeNode": "40",
      "elk.layered.nodePlacement.strategy": "BRANDES_KOEPF",
      // Keep spec order within a layer (Board, Assembly, Firmware read
      // left-to-right); the crossing minimizer otherwise mirrors rows.
      "elk.layered.considerModelOrder.strategy": "NODES_AND_EDGES",
      "elk.layered.crossingMinimization.forceNodeModelOrder": "true",
      "elk.edgeRouting": "ORTHOGONAL",
    },
    children: sized.map((n) => ({ id: n.id, width: n.w, height: n.h })),
    edges: spec.edges.map((e, i) => ({ id: `e${i}`, sources: [e.from], targets: [e.to] })),
  });
  const margin = 40;
  return sized.map((n) => {
    const laid = graph.children!.find((c) => c.id === n.id)!;
    return { ...n, x: Math.round(laid.x! + margin), y: Math.round(laid.y! + margin) };
  });
}

/** Quality gate on the laid-out geometry, mirroring the harness's old checks:
 * no node overlaps or near-collisions, and flow monotone in the layout
 * direction (an edge running backwards means the layering failed). */
function assertLayoutSane(spec: Spec, placed: Placed[]): void {
  const MIN_GAP = 6;
  for (let i = 0; i < placed.length; i++) {
    for (let j = i + 1; j < placed.length; j++) {
      const a = placed[i];
      const b = placed[j];
      const ox = Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x);
      const oy = Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y);
      if (ox > -MIN_GAP && oy > -MIN_GAP) {
        throw new Error(`layout: ${a.id} and ${b.id} overlap or nearly touch`);
      }
    }
  }
  const byId = new Map(placed.map((n) => [n.id, n]));
  for (const e of spec.edges) {
    const s = byId.get(e.from)!;
    const t = byId.get(e.to)!;
    const forward =
      spec.layout.direction === "DOWN" ? t.y >= s.y + s.h : t.x >= s.x + s.w;
    if (!forward) throw new Error(`layout: edge ${e.from}->${e.to} runs backwards`);
  }
}

function drawioXml(name: string, spec: Spec, placed: Placed[]): string {
  const byId = new Map(placed.map((n) => [n.id, n]));
  const outDegree = new Map<string, number>();
  const inDegree = new Map<string, number>();
  for (const e of spec.edges) {
    outDegree.set(e.from, (outDegree.get(e.from) ?? 0) + 1);
    inDegree.set(e.to, (inDegree.get(e.to) ?? 0) + 1);
  }
  const clamp = (v: number) => Math.min(0.85, Math.max(0.15, Math.round(v * 100) / 100));

  const cells: string[] = [];
  for (const n of placed) {
    const { stroke, fill } = STYLE[n.role] ?? STYLE.stage;
    const value = xml(n.label).replaceAll("\n", "&#10;");
    cells.push(
      `        <mxCell id="${n.id}" value="${value}" style="rounded=1;arcSize=8;fillColor=${fill};strokeColor=${stroke};fontColor=#e2e8f0;fontSize=12;" vertex="1" parent="1">\n` +
        `          <mxGeometry x="${n.x}" y="${n.y}" width="${n.w}" height="${n.h}" as="geometry"/>\n` +
        `        </mxCell>`,
    );
  }
  spec.edges.forEach((e, i) => {
    const s = byId.get(e.from)!;
    const t = byId.get(e.to)!;
    const stroke = (STYLE[s.role] ?? STYLE.stage).stroke;
    let anchors: string;
    if (spec.layout.direction === "DOWN") {
      // Fan-out slides the exit toward the target; fan-in slides the entry
      // toward the source; a plain link goes centre-to-centre, the same
      // convention the hand-drawn files use.
      const exitX = (outDegree.get(e.from) ?? 0) > 1 ? clamp((t.x + t.w / 2 - s.x) / s.w) : 0.5;
      const entryX = (inDegree.get(e.to) ?? 0) > 1 ? clamp((s.x + s.w / 2 - t.x) / t.w) : 0.5;
      anchors = `exitX=${exitX};exitY=1;entryX=${entryX};entryY=0;`;
    } else {
      const exitY = (outDegree.get(e.from) ?? 0) > 1 ? clamp((t.y + t.h / 2 - s.y) / s.h) : 0.5;
      const entryY = (inDegree.get(e.to) ?? 0) > 1 ? clamp((s.y + s.h / 2 - t.y) / t.h) : 0.5;
      anchors = `exitX=1;exitY=${exitY};entryX=0;entryY=${entryY};`;
    }
    cells.push(
      `        <mxCell id="e${i}" style="edgeStyle=orthogonalEdgeStyle;rounded=1;strokeColor=${stroke};${anchors}" edge="1" parent="1" source="${e.from}" target="${e.to}">\n` +
        `          <mxGeometry relative="1" as="geometry"/>\n` +
        `        </mxCell>`,
    );
  });

  return (
    `<mxfile host="hauksbee">\n` +
    `  <diagram name="${xml(name)}" id="${xml(name)}">\n` +
    `    <mxGraphModel dx="1100" dy="700" grid="0" page="0" math="0" shadow="0">\n` +
    `      <root>\n` +
    `        <mxCell id="0"/>\n` +
    `        <mxCell id="1" parent="0"/>\n` +
    cells.join("\n") +
    `\n      </root>\n` +
    `    </mxGraphModel>\n` +
    `  </diagram>\n` +
    `</mxfile>\n`
  );
}

const DRAWIO_APP = "/Applications/draw.io.app/Contents/MacOS/draw.io";

async function exportSvg(drawioPath: string, svgPath: string): Promise<void> {
  if (!existsSync(DRAWIO_APP)) {
    throw new Error(
      `draw.io desktop app not found at ${DRAWIO_APP}; install it or export ${drawioPath} by hand`,
    );
  }
  const proc = Bun.spawn([DRAWIO_APP, "-x", "-f", "svg", "-o", svgPath, drawioPath], {
    stdout: "pipe",
    stderr: "pipe",
  });
  const code = await proc.exited;
  if (code !== 0 || !existsSync(svgPath)) {
    const err = await new Response(proc.stderr).text();
    throw new Error(`draw.io export failed for ${drawioPath}: ${err}`);
  }
}

async function lint(paths: string[]): Promise<void> {
  const repo = resolve(import.meta.dir, "../..");
  const proc = Bun.spawn(["python3", "scripts/lint-diagrams.py", ...paths], {
    cwd: repo,
    stdout: "inherit",
    stderr: "inherit",
  });
  if ((await proc.exited) !== 0) throw new Error("scripts/lint-diagrams.py rejected the output");
}

const specsDir = join(import.meta.dir, "specs");
const outDir = resolve(import.meta.dir, "../../docs/assets/diagrams");
const only = process.argv[2];
const specFiles = readdirSync(specsDir)
  .filter((f) => f.endsWith(".json"))
  .filter((f) => !only || basename(f, ".json") === only);
if (specFiles.length === 0) throw new Error(`no spec matches ${only ?? "*"}`);

const written: string[] = [];
for (const file of specFiles) {
  const name = basename(file, ".json");
  const spec: Spec = JSON.parse(readFileSync(join(specsDir, file), "utf8"));
  const placed = await layout(spec);
  assertLayoutSane(spec, placed);
  const drawioPath = join(outDir, `${name}.drawio`);
  const svgPath = join(outDir, `${name}.svg`);
  writeFileSync(drawioPath, drawioXml(name, spec, placed));
  await exportSvg(drawioPath, svgPath);
  written.push(drawioPath);
  console.log(`ok ${name}: ${drawioPath} + ${svgPath}`);
}
await lint(written.map((p) => p.replace(`${resolve(import.meta.dir, "../..")}/`, "")));
console.log("all diagrams generated and linted");
