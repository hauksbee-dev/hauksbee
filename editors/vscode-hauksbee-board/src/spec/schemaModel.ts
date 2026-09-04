// A tiny reader for the GENERATED spec schema
// (schemas/hauksbee-ci-spec.schema.json, produced from the Rust `Spec` type by
// crates/hauksbee-ci/tests/schema_drift.rs).
//
// The keyword vocabulary is closed and known, because we generate the file:
// $ref / definitions, type, properties, additionalProperties, required, enum,
// items, minItems, minimum / maximum / exclusiveMinimum / exclusiveMaximum,
// default, description, format, and `anyOf: [X, {type: "null"}]` for Rust
// Options. A general-purpose validator would buy nothing over that list and
// would have to be bundled into the .vsix.

export interface SchemaNode {
  $ref?: string;
  const?: unknown;
  oneOf?: SchemaNode[];
  allOf?: SchemaNode[];
  type?: string | string[];
  properties?: Record<string, SchemaNode>;
  additionalProperties?: boolean | SchemaNode;
  required?: string[];
  enum?: unknown[];
  items?: SchemaNode;
  minItems?: number;
  maxItems?: number;
  minimum?: number;
  maximum?: number;
  exclusiveMinimum?: number;
  exclusiveMaximum?: number;
  default?: unknown;
  description?: string;
  format?: string;
  anyOf?: SchemaNode[];
  definitions?: Record<string, SchemaNode>;
  title?: string;
}

/** A property as the editor wants to present it. */
export interface PropertyInfo {
  name: string;
  node: SchemaNode;
  description: string;
  required: boolean;
  /** The closed vocabulary for the value, when there is one. */
  enumValues: string[];
  /** Per-value documentation, when the schema carries it (Rust unit enums do). */
  enumDocs: Record<string, string>;
  /** "string" | "number" | "boolean" | "table" | "array of table" | ... */
  typeLabel: string;
  /** True when the value is a table (or array of tables) the user writes as `[x]`. */
  isTable: boolean;
  isArrayOfTables: boolean;
}

export class SpecSchema {
  constructor(private readonly root: SchemaNode) {}

  /** Follow `$ref` and collapse `anyOf: [X, null]` (a Rust `Option<X>`). */
  resolve(node: SchemaNode | undefined): SchemaNode | undefined {
    if (!node) return undefined;
    if (node.$ref) {
      const name = node.$ref.replace(/^#\/definitions\//, "");
      const def = this.root.definitions?.[name];
      // Carry the referring node's description: schemars puts the doc comment on
      // the property, and the definition's own title is less useful for a hover.
      if (def) return { ...def, description: node.description ?? def.description };
      return undefined;
    }
    // `allOf: [{$ref}]` is how schemars attaches a doc comment to a $ref'd
    // field (a Rust enum like `DnpMode`); the description lives on the wrapper.
    if (node.allOf?.length === 1) {
      const inner = this.resolve(node.allOf[0]);
      if (inner) return { ...inner, description: node.description ?? inner.description };
    }
    if (node.anyOf && node.anyOf.some((a) => a.type === "null")) {
      const real = node.anyOf.find((a) => a.type !== "null");
      if (real) {
        const resolved = this.resolve(real);
        if (resolved) return { ...resolved, description: node.description ?? resolved.description };
      }
    }
    return node;
  }

  /** The node for a table path: `["supply"]`, `["peripheral", "event"]`, `["ac"]`. */
  nodeAt(schemaPath: string[]): SchemaNode | undefined {
    let node: SchemaNode | undefined = this.root;
    for (const seg of schemaPath) {
      node = this.resolve(node);
      const prop = node?.properties?.[seg];
      if (!prop) return undefined;
      node = this.resolve(prop);
      // A `[[supply]]` header addresses one ELEMENT of the array property.
      if (node && typeIncludes(node, "array") && node.items) node = this.resolve(node.items);
    }
    return this.resolve(node);
  }

  /** Every property valid inside the table at `schemaPath`. */
  propertiesAt(schemaPath: string[]): PropertyInfo[] {
    const node = this.nodeAt(schemaPath);
    if (!node?.properties) return [];
    const required = new Set(node.required ?? []);
    return Object.entries(node.properties).map(([name, raw]) =>
      this.describe(name, raw, required.has(name))
    );
  }

  property(schemaPath: string[], key: string): PropertyInfo | undefined {
    const node = this.nodeAt(schemaPath);
    const raw = node?.properties?.[key];
    if (!raw) return undefined;
    return this.describe(key, raw, (node?.required ?? []).includes(key));
  }

  private describe(name: string, raw: SchemaNode, required: boolean): PropertyInfo {
    const resolved = this.resolve(raw) ?? {};
    const isArray = typeIncludes(resolved, "array");
    const item = isArray ? this.resolve(resolved.items) : undefined;
    const inner = isArray ? item : resolved;
    const isTable = !!inner?.properties;
    const vocabulary = enumOf(resolved).length ? enumOf(resolved) : enumOf(inner);
    const docs = { ...enumDocsOf(inner), ...enumDocsOf(resolved) };
    return {
      name,
      node: resolved,
      description: raw.description ?? resolved.description ?? "",
      required,
      enumValues: vocabulary,
      enumDocs: docs,
      typeLabel: label(resolved, inner),
      isTable: isTable && !isArray,
      isArrayOfTables: isTable && isArray,
    };
  }

  /** Root-level tables and array-of-tables, for `[` completions. */
  tablePathsUnder(schemaPath: string[]): PropertyInfo[] {
    return this.propertiesAt(schemaPath).filter((p) => p.isTable || p.isArrayOfTables);
  }

  get description(): string {
    return this.root.description ?? "";
  }
}

/**
 * The closed set of string values a node accepts. schemars writes a plain
 * `enum` for the `#[schemars(extend("enum" = [...]))]` string fields, and a
 * `oneOf` of `const`s for a real Rust unit enum (`DnpMode`), so both spellings
 * have to be read or a typo in `dnp` would go unflagged.
 */
export function enumOf(node: SchemaNode | undefined): string[] {
  if (!node) return [];
  const direct = (node.enum ?? []).filter((v): v is string => typeof v === "string");
  if (direct.length) return direct;
  const variants = node.oneOf ?? node.anyOf ?? [];
  return variants
    .map((v) => v.const)
    .filter((v): v is string => typeof v === "string");
}

function enumDocsOf(node: SchemaNode | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  for (const v of node?.oneOf ?? node?.anyOf ?? []) {
    if (typeof v.const === "string" && v.description) out[v.const] = v.description;
  }
  return out;
}

export function typeIncludes(node: SchemaNode | undefined, t: string): boolean {
  if (!node?.type) return false;
  return Array.isArray(node.type) ? node.type.includes(t) : node.type === t;
}

export function allowsNull(node: SchemaNode | undefined): boolean {
  if (!node) return true;
  if (typeIncludes(node, "null")) return true;
  return !!node.anyOf?.some((a) => a.type === "null");
}

function label(node: SchemaNode, inner: SchemaNode | undefined): string {
  if (typeIncludes(node, "array")) {
    if (inner?.properties) return "array of tables";
    return `array of ${scalarLabel(inner)}`;
  }
  if (node.properties) return "table";
  return scalarLabel(node);
}

function scalarLabel(node: SchemaNode | undefined): string {
  if (!node?.type) return enumOf(node).length ? "string" : "value";
  const types = (Array.isArray(node.type) ? node.type : [node.type]).filter((t) => t !== "null");
  if (types.length === 0) return "value";
  return types.join(" | ");
}

/**
 * A number that SATISFIES the node's bounds, for seeding an inserted key or a
 * snippet placeholder. Seeding a blanket `0` put four errors on the `[ac]` block
 * the moment it was scaffolded, which is the opposite of helpful.
 */
export function seedNumber(node: SchemaNode): number {
  const integer = typeIncludes(node, "integer");
  const step = integer ? 1 : 1;
  let v = 0;
  if (node.minimum !== undefined) v = Math.max(v, node.minimum);
  if (node.exclusiveMinimum !== undefined) v = Math.max(v, node.exclusiveMinimum + step);
  if (node.maximum !== undefined) v = Math.min(v, node.maximum);
  if (node.exclusiveMaximum !== undefined) v = Math.min(v, node.exclusiveMaximum - step);
  return v;
}

/** A human note on the numeric bounds a node carries, for hovers. */
export function boundsNote(node: SchemaNode): string {
  const bits: string[] = [];
  if (node.exclusiveMinimum !== undefined) bits.push(`> ${node.exclusiveMinimum}`);
  else if (node.minimum !== undefined) bits.push(`>= ${node.minimum}`);
  if (node.exclusiveMaximum !== undefined) bits.push(`< ${node.exclusiveMaximum}`);
  else if (node.maximum !== undefined) bits.push(`<= ${node.maximum}`);
  return bits.join(", ");
}
