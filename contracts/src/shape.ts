/**
 * Minimal runtime shape checker for contract tests.
 *
 * A spec is either:
 *  - a primitive descriptor string: "string" | "number" | "boolean" | "any",
 *    optionally union'd with null ("string|null") and/or suffixed with "?"
 *    (key may be absent),
 *  - an object mapping keys to specs (extra keys in the value are ERRORS —
 *    the contract is frozen, additions must be explicit),
 *  - a single-element array [spec] describing every element of an array.
 *
 * Throws with a dotted path to the first mismatch so failures read like:
 *   "account.credits_balance: expected number, got string"
 */

export type Spec = string | { [key: string]: Spec } | [Spec];

export function assertShape(value: unknown, spec: Spec, path = "$"): void {
  if (typeof spec === "string") {
    assertPrimitive(value, spec, path);
    return;
  }

  if (Array.isArray(spec)) {
    if (!Array.isArray(value)) {
      throw new Error(`${path}: expected array, got ${describe(value)}`);
    }
    value.forEach((item, i) => assertShape(item, spec[0], `${path}[${i}]`));
    return;
  }

  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${path}: expected object, got ${describe(value)}`);
  }

  const obj = value as Record<string, unknown>;
  for (const [key, keySpec] of Object.entries(spec)) {
    const optional = typeof keySpec === "string" && keySpec.endsWith("?");
    if (!(key in obj)) {
      if (optional) continue;
      throw new Error(`${path}.${key}: missing required key`);
    }
    const childSpec =
      typeof keySpec === "string" && optional ? keySpec.slice(0, -1) : keySpec;
    assertShape(obj[key], childSpec, `${path}.${key}`);
  }

  for (const key of Object.keys(obj)) {
    if (!(key in spec)) {
      throw new Error(
        `${path}.${key}: unexpected key (contract is frozen — if this addition is intentional, update the contract tests)`,
      );
    }
  }
}

// Standard ISO8601 UTC (the SaaS API's timestamp format since SCR-87 I3).
const ISO8601 =
  /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$/;

function assertPrimitive(value: unknown, spec: string, path: string): void {
  const alternatives = spec.split("|");
  for (const alt of alternatives) {
    if (alt === "any") return;
    if (alt === "null" && value === null) return;
    if (alt === "string" && typeof value === "string") return;
    if (alt === "number" && typeof value === "number") return;
    if (alt === "boolean" && typeof value === "boolean") return;
    if (alt === "timestamp" && typeof value === "string" && ISO8601.test(value))
      return;
  }
  throw new Error(`${path}: expected ${spec}, got ${describe(value)}`);
}

function describe(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  return typeof value;
}
