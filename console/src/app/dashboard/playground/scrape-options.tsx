"use client";

import { useState } from "react";
import { z } from "zod";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import type { OcrMode, RequestCookie, ScrapeAction } from "@/lib/api-types";
import { MonitorSmartphone, Plus, X } from "lucide-react";

export interface CookieRow {
  name: string;
  value: string;
  domain: string;
}

export interface ScrapeState {
  formats: string[];
  only_main_content: boolean;
  include_links: boolean;
  timeout_ms: string;
  /** Screenshot the whole scrollable page (true) or only the viewport */
  screenshot_full_page: boolean;
  /** Render JavaScript (forced on by screenshot, actions and mobile) */
  render_js: boolean;
  /** Emulate a phone */
  mobile: boolean;
  // Page actions (JSON array)
  feat_actions: boolean;
  actions_json: string;
  // Cookies
  feat_cookies: boolean;
  cookies: CookieRow[];
  ai_summary: boolean;
  // Schema extraction
  feat_schema: boolean;
  // Block splitting
  feat_block_split: boolean;
  // Custom CSS selectors
  feat_custom_selectors: boolean;
  custom_selectors: string; // JSON string: { field: "selector" }
  // AI extraction
  feat_ai_extraction: boolean;
  ai_extraction_prompt: string;
  // Documents (PDF, office formats) and OCR
  ocr_mode: OcrMode;
  ocr_max_pages: string;
  max_pages: string;
}

/** Where the content comes from: a URL (`/scrape`) or an uploaded file (`/parse`). */
export type ScrapeSource = "url" | "file";

interface ScrapeOptionsProps {
  state: ScrapeState;
  onChange: (state: ScrapeState) => void;
  source: ScrapeSource;
  /** URL being scraped, to check cookie domains against its host */
  targetUrl?: string;
}

// ============================================================================
// Browser-only features
// ============================================================================

/** The enabled features that make the engine render the page in a browser. */
export function browserReasons(state: ScrapeState): string[] {
  const reasons: string[] = [];
  if (state.formats.includes("screenshot")) reasons.push("screenshot");
  if (state.feat_actions && state.actions_json.trim()) reasons.push("actions");
  if (state.mobile) reasons.push("mobile");
  return reasons;
}

// ============================================================================
// Page actions: validated JSON (mirrors scrapix_core::browser::Action)
// ============================================================================

const MAX_ACTIONS = 50;
const ACTION_TYPES = [
  "wait",
  "click",
  "scroll",
  "write",
  "press",
  "execute_javascript",
] as const;
type ActionType = (typeof ACTION_TYPES)[number];

export const ACTIONS_EXAMPLE = `[
  { "type": "wait", "selector": "main" },
  { "type": "click", "selector": "button.accept-cookies" },
  { "type": "scroll", "direction": "down", "amount": 2 },
  { "type": "wait", "ms": 500 },
  { "type": "execute_javascript", "script": "return document.title" }
]`;

/** A string field, with "is required" when it is missing. */
function stringField(field: string) {
  return z.string({
    error: (issue) =>
      issue.input === undefined
        ? `\`${field}\` is required`
        : `\`${field}\` must be a string`,
  });
}

const selectorSchema = stringField("selector")
  .trim()
  .min(1, "selector must not be empty")
  .max(1024, "selector is too long (max 1024 characters)");

const actionSchemas: Record<ActionType, z.ZodType<ScrapeAction>> = {
  wait: z
    .object({
      type: z.literal("wait"),
      ms: z
        .number({ error: "ms must be a number" })
        .int("ms must be an integer")
        .min(0, "ms must not be negative")
        .max(30_000, "ms must be at most 30000")
        .optional(),
      selector: selectorSchema.optional(),
    })
    .refine((a) => (a.ms === undefined) !== (a.selector === undefined), {
      message: "exactly one of `ms` or `selector` must be set",
    }),
  click: z.object({ type: z.literal("click"), selector: selectorSchema }),
  scroll: z.object({
    type: z.literal("scroll"),
    direction: z
      .enum(["up", "down"], { error: "direction must be `up` or `down`" })
      .optional(),
    amount: z
      .number({ error: "amount must be a number" })
      .gt(0, "amount must be a number of screens in (0, 100]")
      .max(100, "amount must be a number of screens in (0, 100]")
      .optional(),
  }),
  write: z.object({
    type: z.literal("write"),
    selector: selectorSchema,
    text: stringField("text").max(10 * 1024, "text is too long (max 10 KB)"),
  }),
  press: z.object({
    type: z.literal("press"),
    key: stringField("key")
      .min(1, "key must be a key name such as `Enter` or `a`")
      .max(32, "key must be a key name such as `Enter` or `a`"),
  }),
  execute_javascript: z.object({
    type: z.literal("execute_javascript"),
    script: stringField("script")
      .trim()
      .min(1, "script must not be empty")
      .max(100 * 1024, "script is too long (max 100 KB)"),
  }),
};

function isActionType(value: unknown): value is ActionType {
  return (
    typeof value === "string" &&
    (ACTION_TYPES as readonly string[]).includes(value)
  );
}

export type ParsedActions =
  | { ok: true; actions: ScrapeAction[] }
  | { ok: false; errors: string[] };

/** Parse and validate the actions JSON. Errors name the action's index and type. */
export function parseActions(text: string): ParsedActions {
  if (!text.trim()) return { ok: true, actions: [] };
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch (e) {
    return {
      ok: false,
      errors: [`Invalid JSON: ${e instanceof Error ? e.message : String(e)}`],
    };
  }
  if (!Array.isArray(raw)) {
    return { ok: false, errors: ["Actions must be a JSON array"] };
  }
  if (raw.length > MAX_ACTIONS) {
    return {
      ok: false,
      errors: [`Too many actions (${raw.length}); at most ${MAX_ACTIONS}`],
    };
  }
  const actions: ScrapeAction[] = [];
  const errors: string[] = [];
  raw.forEach((item: unknown, index) => {
    const type =
      typeof item === "object" && item !== null
        ? (item as Record<string, unknown>).type
        : undefined;
    if (!isActionType(type)) {
      errors.push(
        `actions[${index}]: unknown type ${JSON.stringify(type ?? null)} (expected ${ACTION_TYPES.join(", ")})`,
      );
      return;
    }
    const result = actionSchemas[type].safeParse(item);
    if (result.success) {
      actions.push(result.data);
    } else {
      const issue = result.error.issues[0];
      errors.push(`actions[${index}] (${type}): ${issue?.message ?? "invalid"}`);
    }
  });
  return errors.length > 0 ? { ok: false, errors } : { ok: true, actions };
}

// ============================================================================
// Cookies (mirrors RequestCookie::validate_for)
// ============================================================================

const COOKIE_NAME_RE = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
// RFC 6265 cookie-octet plus space: no control chars, quotes, commas, semicolons or backslashes
const COOKIE_VALUE_RE = /^[\x20\x21\x23-\x2B\x2D-\x3A\x3C-\x5B\x5D-\x7E]*$/;

function hostOf(url: string | undefined): string | null {
  if (!url) return null;
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return null;
  }
}

function isEmptyCookieRow(row: CookieRow): boolean {
  return !row.name.trim() && !row.value.trim() && !row.domain.trim();
}

/** Why a cookie row is invalid for `targetUrl`, or null. Empty rows are ignored. */
export function cookieRowError(row: CookieRow, targetUrl?: string): string | null {
  if (isEmptyCookieRow(row)) return null;
  const name = row.name.trim();
  if (!name) return "Name is required";
  if (!COOKIE_NAME_RE.test(name)) return `Invalid cookie name "${name}"`;
  if (!COOKIE_VALUE_RE.test(row.value)) {
    return "Value can't contain quotes, commas, semicolons, backslashes or control characters";
  }
  if (name.length + row.value.length > 4096) {
    return "Cookie is too large (max 4096 bytes)";
  }
  const domain = row.domain.trim().replace(/^\./, "").toLowerCase();
  if (domain) {
    if (!domain.includes(".")) return `"${domain}" is not a valid cookie domain`;
    const host = hostOf(targetUrl);
    if (host && host !== domain && !host.endsWith(`.${domain}`)) {
      return `Domain "${domain}" does not match the target host "${host}"`;
    }
  }
  return null;
}

/** The non-empty rows as API cookies. */
export function buildCookies(rows: CookieRow[]): RequestCookie[] {
  return rows
    .filter((r) => !isEmptyCookieRow(r))
    .map((r) => {
      const cookie: RequestCookie = { name: r.name.trim(), value: r.value };
      const domain = r.domain.trim();
      if (domain) cookie.domain = domain;
      return cookie;
    });
}

/** Formats that apply to documents; the rest only exist for HTML pages. */
export const DOCUMENT_FORMATS = ["markdown", "content", "links", "metadata"];

const OCR_MODES: { value: OcrMode; label: string; description: string }[] = [
  {
    value: "off",
    label: "Off",
    description: "Scanned pages are flagged, not read",
  },
  {
    value: "auto",
    label: "Auto",
    description: "OCR only the pages with no text layer",
  },
  {
    value: "force",
    label: "Force",
    description: "OCR every page (for garbled text)",
  },
];

const FORMAT_OPTIONS: {
  value: string;
  label: string;
  description: string;
  /** Needs a browser (turns on JS rendering) */
  browser?: boolean;
}[] = [
  { value: "markdown", label: "Markdown", description: "Clean, readable text with formatting" },
  { value: "html", label: "HTML", description: "Cleaned HTML with main content" },
  { value: "rawhtml", label: "Raw HTML", description: "Original unprocessed HTML source" },
  { value: "content", label: "Content", description: "Plain text without any markup" },
  { value: "links", label: "Links", description: "All hyperlinks found on the page" },
  { value: "metadata", label: "Metadata", description: "Title, description, OG tags, etc." },
  { value: "screenshot", label: "Screenshot", description: "PNG of the rendered page", browser: true },
];

function BrowserBadge() {
  return (
    <Badge
      variant="outline"
      className="text-[10px] px-1.5 py-0 font-normal text-muted-foreground"
      title="Renders the page in a browser (turns on JS rendering)"
    >
      JS
    </Badge>
  );
}

function SwitchRow({
  id,
  label,
  description,
  checked,
  onCheckedChange,
  browser,
  disabled,
}: {
  id: string;
  label: string;
  description?: string;
  checked: boolean;
  onCheckedChange: (v: boolean) => void;
  /** Show the "JS" badge: this option renders the page in a browser */
  browser?: boolean;
  disabled?: boolean;
}) {
  return (
    <div className="flex items-center justify-between gap-3">
      <div>
        <div className="flex items-center gap-1.5">
          <Label htmlFor={id} className="text-sm font-medium">
            {label}
          </Label>
          {browser && <BrowserBadge />}
        </div>
        {description && (
          <p className="text-xs text-muted-foreground">{description}</p>
        )}
      </div>
      <Switch
        id={id}
        checked={checked}
        onCheckedChange={onCheckedChange}
        disabled={disabled}
      />
    </div>
  );
}

function KeyValueList({
  label,
  description,
  keyPlaceholder,
  valuePlaceholder,
  value,
  onChange,
}: {
  label: string;
  description: string;
  keyPlaceholder: string;
  valuePlaceholder: string;
  value: string;
  onChange: (value: string) => void;
}) {
  const [key, setKey] = useState("");
  const [val, setVal] = useState("");

  const entries: [string, string][] = (() => {
    if (!value.trim()) return [];
    try {
      return Object.entries(JSON.parse(value)) as [string, string][];
    } catch {
      return [];
    }
  })();

  const addEntry = () => {
    const k = key.trim();
    const v = val.trim();
    if (!k || !v) return;
    const obj = Object.fromEntries(entries);
    obj[k] = v;
    onChange(JSON.stringify(obj));
    setKey("");
    setVal("");
  };

  const removeEntry = (k: string) => {
    const obj = Object.fromEntries(entries.filter(([ek]) => ek !== k));
    onChange(Object.keys(obj).length > 0 ? JSON.stringify(obj) : "");
  };

  return (
    <div className="space-y-2">
      <div>
        <Label className="text-sm font-medium">{label}</Label>
        <p className="text-xs text-muted-foreground">{description}</p>
      </div>

      {entries.length > 0 && (
        <div className="space-y-1">
          {entries.map(([k, v]) => (
            <div
              key={k}
              className="flex items-center gap-2 rounded-md bg-secondary/50 px-2.5 py-1.5"
            >
              <span className="text-xs font-medium shrink-0">{k}</span>
              <span className="text-muted-foreground text-xs">&rarr;</span>
              <span className="text-xs font-mono text-muted-foreground flex-1 truncate">
                {v}
              </span>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                className="h-4 w-4 rounded-full p-0 hover:bg-muted-foreground/20 shrink-0"
                onClick={() => removeEntry(k)}
              >
                <X className="h-3 w-3" />
              </Button>
            </div>
          ))}
        </div>
      )}

      <div className="flex gap-1.5">
        <Input
          placeholder={keyPlaceholder}
          value={key}
          onChange={(e) => setKey(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              addEntry();
            }
          }}
          className="flex-1 text-xs"
        />
        <Input
          placeholder={valuePlaceholder}
          value={val}
          onChange={(e) => setVal(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              addEntry();
            }
          }}
          className="flex-1 font-mono text-xs"
        />
        <Button
          type="button"
          variant="outline"
          size="icon"
          className="shrink-0 h-9 w-9"
          onClick={addEntry}
          disabled={!key.trim() || !val.trim()}
        >
          <Plus className="h-3.5 w-3.5" />
        </Button>
      </div>
    </div>
  );
}

function ActionsEditor({
  value,
  onChange,
}: {
  value: string;
  onChange: (value: string) => void;
}) {
  const parsed = parseActions(value);
  return (
    <div className="space-y-1.5">
      <div className="flex items-center justify-between gap-2">
        <Label htmlFor="actions-json" className="text-sm font-medium">
          Actions (JSON)
        </Label>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-6 px-2 text-xs"
          onClick={() => onChange(ACTIONS_EXAMPLE)}
        >
          Insert example
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">
        Run in order before capture: <code className="font-mono">wait</code>,{" "}
        <code className="font-mono">click</code>,{" "}
        <code className="font-mono">scroll</code>,{" "}
        <code className="font-mono">write</code>,{" "}
        <code className="font-mono">press</code>,{" "}
        <code className="font-mono">execute_javascript</code>. Max 50, 30s in
        total.
      </p>
      <Textarea
        id="actions-json"
        rows={7}
        spellCheck={false}
        className="font-mono text-xs"
        placeholder={'[\n  { "type": "click", "selector": "#load-more" },\n  { "type": "wait", "ms": 1000 }\n]'}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        aria-invalid={!parsed.ok ? true : undefined}
      />
      {parsed.ok ? (
        parsed.actions.length > 0 && (
          <p className="text-xs text-muted-foreground">
            {parsed.actions.length} valid action
            {parsed.actions.length === 1 ? "" : "s"}
          </p>
        )
      ) : (
        <ul className="space-y-0.5">
          {parsed.errors.slice(0, 5).map((err) => (
            <li
              key={err}
              className="text-xs text-destructive font-mono break-words"
            >
              {err}
            </li>
          ))}
          {parsed.errors.length > 5 && (
            <li className="text-xs text-destructive">
              and {parsed.errors.length - 5} more
            </li>
          )}
        </ul>
      )}
    </div>
  );
}

function CookiesEditor({
  rows,
  onChange,
  targetUrl,
}: {
  rows: CookieRow[];
  onChange: (rows: CookieRow[]) => void;
  targetUrl?: string;
}) {
  const update = (index: number, patch: Partial<CookieRow>) =>
    onChange(rows.map((r, i) => (i === index ? { ...r, ...patch } : r)));
  const remove = (index: number) =>
    onChange(rows.filter((_, i) => i !== index));

  return (
    <div className="space-y-2">
      <div>
        <Label className="text-sm font-medium">Cookies</Label>
        <p className="text-xs text-muted-foreground">
          Domain is optional (defaults to the target host). Max 50.
        </p>
      </div>
      {rows.map((row, i) => {
        const error = cookieRowError(row, targetUrl);
        return (
          <div key={i} className="space-y-1">
            <div className="flex gap-1.5">
              <Input
                placeholder="name"
                aria-label={`Cookie ${i + 1} name`}
                value={row.name}
                onChange={(e) => update(i, { name: e.target.value })}
                aria-invalid={error ? true : undefined}
                className="flex-1 min-w-0 font-mono text-xs"
              />
              <Input
                placeholder="value"
                aria-label={`Cookie ${i + 1} value`}
                value={row.value}
                onChange={(e) => update(i, { value: e.target.value })}
                aria-invalid={error ? true : undefined}
                className="flex-1 min-w-0 font-mono text-xs"
              />
              <Input
                placeholder="domain"
                aria-label={`Cookie ${i + 1} domain`}
                value={row.domain}
                onChange={(e) => update(i, { domain: e.target.value })}
                aria-invalid={error ? true : undefined}
                className="flex-1 min-w-0 font-mono text-xs"
              />
              <Button
                type="button"
                variant="ghost"
                size="icon"
                className="h-9 w-9 shrink-0"
                aria-label={`Remove cookie ${i + 1}`}
                onClick={() => remove(i)}
              >
                <X className="h-3.5 w-3.5" />
              </Button>
            </div>
            {error && <p className="text-xs text-destructive">{error}</p>}
          </div>
        );
      })}
      <Button
        type="button"
        variant="outline"
        size="sm"
        className="h-8 text-xs"
        disabled={rows.length >= 50}
        onClick={() => onChange([...rows, { name: "", value: "", domain: "" }])}
      >
        <Plus className="mr-1 h-3.5 w-3.5" />
        Add cookie
      </Button>
    </div>
  );
}

export function ScrapeOptions({ state, onChange, source, targetUrl }: ScrapeOptionsProps) {
  const isFile = source === "file";
  const formatOptions = isFile
    ? FORMAT_OPTIONS.filter((f) => DOCUMENT_FORMATS.includes(f.value))
    : FORMAT_OPTIONS;
  const set = <K extends keyof ScrapeState>(key: K, value: ScrapeState[K]) =>
    onChange({ ...state, [key]: value });

  const toggle = (value: string) => {
    const formats = state.formats.includes(value)
      ? state.formats.filter((f) => f !== value)
      : [...state.formats, value];
    onChange({ ...state, formats });
  };

  const forcedBy = browserReasons(state);
  const jsForced = forcedBy.length > 0;

  return (
    <div className="space-y-5">
      <div className="space-y-3">
        <Label className="text-xs text-muted-foreground uppercase tracking-wide">
          Output Formats
        </Label>
        <div className="space-y-1">
          {formatOptions.map(({ value, label, description, browser }) => (
            <SwitchRow
              key={value}
              id={`fmt-${value}`}
              label={label}
              description={description}
              browser={browser}
              checked={state.formats.includes(value)}
              onCheckedChange={() => toggle(value)}
            />
          ))}
        </div>
        {state.formats.includes("screenshot") && (
          <div className="flex items-center justify-between gap-3 pl-2 border-l-2 border-primary/20 ml-1">
            <Label className="text-sm font-medium">Screenshot area</Label>
            <ToggleGroup
              type="single"
              variant="outline"
              size="sm"
              value={state.screenshot_full_page ? "full" : "viewport"}
              onValueChange={(v) => {
                if (v) set("screenshot_full_page", v === "full");
              }}
            >
              <ToggleGroupItem value="full" className="text-xs px-2.5">
                Full page
              </ToggleGroupItem>
              <ToggleGroupItem value="viewport" className="text-xs px-2.5">
                Viewport
              </ToggleGroupItem>
            </ToggleGroup>
          </div>
        )}
      </div>

      {/* ── Browser (URLs only) ── */}
      {!isFile && (
        <div className="space-y-3 border-t pt-4">
          <Label className="text-xs text-muted-foreground uppercase tracking-wide">
            Browser
          </Label>

          <div className="space-y-1">
            <SwitchRow
              id="render-js"
              label="Render JavaScript"
              description={
                jsForced
                  ? `Turned on by ${forcedBy.join(", ")}`
                  : "Load the page in a headless browser"
              }
              checked={state.render_js || jsForced}
              disabled={jsForced}
              onCheckedChange={(v) => set("render_js", v)}
            />
            {jsForced && (
              <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
                <MonitorSmartphone className="h-3 w-3 shrink-0" />
                Options marked JS always render in a browser.
              </p>
            )}
          </div>

          <SwitchRow
            id="mobile"
            label="Mobile"
            description="Phone viewport, touch and mobile user agent"
            browser
            checked={state.mobile}
            onCheckedChange={(v) => set("mobile", v)}
          />

          <SwitchRow
            id="feat-actions"
            label="Page actions"
            description="Click, type, scroll or run JS before capture"
            browser
            checked={state.feat_actions}
            onCheckedChange={(v) => set("feat_actions", v)}
          />
          {state.feat_actions && (
            <div className="pl-2 border-l-2 border-primary/20 ml-1">
              <ActionsEditor
                value={state.actions_json}
                onChange={(v) => set("actions_json", v)}
              />
            </div>
          )}

          <SwitchRow
            id="feat-cookies"
            label="Cookies"
            description="Send cookies with the request"
            checked={state.feat_cookies}
            onCheckedChange={(v) =>
              onChange({
                ...state,
                feat_cookies: v,
                cookies:
                  v && state.cookies.length === 0
                    ? [{ name: "", value: "", domain: "" }]
                    : state.cookies,
              })
            }
          />
          {state.feat_cookies && (
            <div className="pl-2 border-l-2 border-primary/20 ml-1">
              <CookiesEditor
                rows={state.cookies}
                onChange={(rows) => set("cookies", rows)}
                targetUrl={targetUrl}
              />
            </div>
          )}
        </div>
      )}

      {/* ── Documents ── */}
      <div className="space-y-3 border-t pt-4">
        <div>
          <Label className="text-xs text-muted-foreground uppercase tracking-wide">
            Documents
          </Label>
          <p className="text-xs text-muted-foreground mt-1">
            {isFile
              ? "PDF, Word, Excel, PowerPoint, OpenDocument, RTF, EPUB, CSV, or an image with OCR."
              : "Applies when the URL serves a PDF or an office document (Word, Excel, PowerPoint, ...)."}
          </p>
        </div>

        <div className="space-y-1.5">
          <Label className="text-sm font-medium">OCR</Label>
          <ToggleGroup
            type="single"
            variant="outline"
            value={state.ocr_mode}
            onValueChange={(v) => {
              if (v) set("ocr_mode", v as OcrMode);
            }}
            className="w-full"
          >
            {OCR_MODES.map((m) => (
              <ToggleGroupItem
                key={m.value}
                value={m.value}
                className="flex-1 data-[state=on]:bg-primary/10 data-[state=on]:text-primary data-[state=on]:border-primary/30"
              >
                {m.label}
              </ToggleGroupItem>
            ))}
          </ToggleGroup>
          <p className="text-xs text-muted-foreground">
            {OCR_MODES.find((m) => m.value === state.ocr_mode)?.description}
            {state.ocr_mode !== "off" && " · 5 credits per OCR'd page"}
          </p>
        </div>

        <div className="grid grid-cols-2 gap-3">
          {state.ocr_mode !== "off" && (
            <div className="space-y-1.5">
              <Label htmlFor="ocr-max-pages" className="text-sm font-medium">
                OCR page cap
              </Label>
              <Input
                id="ocr-max-pages"
                type="number"
                min="1"
                placeholder="50"
                value={state.ocr_max_pages}
                onChange={(e) => set("ocr_max_pages", e.target.value)}
              />
            </div>
          )}
          <div className="space-y-1.5">
            <Label htmlFor="max-pages" className="text-sm font-medium">
              Max PDF pages
            </Label>
            <Input
              id="max-pages"
              type="number"
              min="1"
              placeholder="All"
              value={state.max_pages}
              onChange={(e) => set("max_pages", e.target.value)}
            />
          </div>
        </div>
      </div>

      {/* ── Features ── */}
      {!isFile && (
      <div className="space-y-3 border-t pt-4">
        <Label className="text-xs text-muted-foreground uppercase tracking-wide">
          Features
        </Label>

        {/* Schema extraction */}
        <SwitchRow
          id="feat-schema"
          label="Schema extraction"
          description="JSON-LD, Microdata, RDFa"
          checked={state.feat_schema}
          onCheckedChange={(v) => set("feat_schema", v)}
        />

        {/* Block splitting */}
        <SwitchRow
          id="feat-block-split"
          label="Block splitting"
          description="Split content into semantic blocks"
          checked={state.feat_block_split}
          onCheckedChange={(v) => set("feat_block_split", v)}
        />

        {/* Custom CSS Selectors */}
        <SwitchRow
          id="feat-selectors"
          label="Custom CSS selectors"
          description="Extract content with CSS selectors"
          checked={state.feat_custom_selectors}
          onCheckedChange={(v) => set("feat_custom_selectors", v)}
        />
        {state.feat_custom_selectors && (
          <div className="pl-1 border-l-2 border-primary/20 ml-1">
            <KeyValueList
              label="Selectors"
              description="Map field names to CSS selectors."
              keyPlaceholder="field"
              valuePlaceholder=".css-selector"
              value={state.custom_selectors}
              onChange={(v) => set("custom_selectors", v)}
            />
          </div>
        )}
      </div>
      )}

      {/* ── AI ── */}
      <div className="space-y-3 border-t pt-4">
        <Label className="text-xs text-muted-foreground uppercase tracking-wide">
          AI
        </Label>

        <SwitchRow
          id="ai-summary"
          label="AI Summary"
          description="Generate a TL;DR using Claude Haiku"
          checked={state.ai_summary}
          onCheckedChange={(v) => set("ai_summary", v)}
        />

        {/* AI Extraction */}
        <SwitchRow
          id="feat-ai-extraction"
          label="AI extraction"
          description="Use LLM to extract structured data"
          checked={state.feat_ai_extraction}
          onCheckedChange={(v) => set("feat_ai_extraction", v)}
        />
        {state.feat_ai_extraction && (
          <div className="space-y-3 pl-1 border-l-2 border-primary/20 ml-1">
            <div className="space-y-1.5">
              <Label htmlFor="ai-prompt" className="text-sm font-medium">
                Prompt
              </Label>
              <Textarea
                id="ai-prompt"
                placeholder="Extract the product name, price, and description from this page."
                value={state.ai_extraction_prompt}
                onChange={(e) => set("ai_extraction_prompt", e.target.value)}
                rows={3}
              />
            </div>
          </div>
        )}
      </div>

      {/* ── Options ── */}
      <div className="space-y-3 border-t pt-4">
        <Label className="text-xs text-muted-foreground uppercase tracking-wide">
          Options
        </Label>

        {!isFile && (
        <div className="flex items-center justify-between">
          <div>
            <Label htmlFor="main-content" className="text-sm font-medium">
              Main content only
            </Label>
            <p className="text-xs text-muted-foreground">
              Exclude navigation, footer, sidebar
            </p>
          </div>
          <Switch
            id="main-content"
            checked={state.only_main_content}
            onCheckedChange={(v) => set("only_main_content", v)}
          />
        </div>
        )}

        <div className="flex items-center justify-between">
          <div>
            <Label htmlFor="include-links" className="text-sm font-medium">
              Include links
            </Label>
            <p className="text-xs text-muted-foreground">
              {isFile
                ? "Extract all links found in the document"
                : "Extract all links found on the page"}
            </p>
          </div>
          <Switch
            id="include-links"
            checked={state.include_links}
            onCheckedChange={(v) => set("include_links", v)}
          />
        </div>

        {!isFile && (
        <div className="space-y-2">
          <Label htmlFor="timeout" className="text-sm font-medium">
            Timeout (ms)
          </Label>
          <Input
            id="timeout"
            type="number"
            min="1000"
            max="120000"
            value={state.timeout_ms}
            onChange={(e) => set("timeout_ms", e.target.value)}
            className="w-full"
          />
        </div>
        )}
      </div>
    </div>
  );
}
