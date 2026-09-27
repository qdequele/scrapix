"use client";

import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { Plus, X } from "lucide-react";
import type { OcrMode } from "@/lib/api-types";

export interface ScrapeState {
  formats: string[];
  only_main_content: boolean;
  include_links: boolean;
  timeout_ms: string;
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
}[] = [
  { value: "markdown", label: "Markdown", description: "Clean, readable text with formatting" },
  { value: "html", label: "HTML", description: "Cleaned HTML with main content" },
  { value: "rawhtml", label: "Raw HTML", description: "Original unprocessed HTML source" },
  { value: "content", label: "Content", description: "Plain text without any markup" },
  { value: "links", label: "Links", description: "All hyperlinks found on the page" },
  { value: "metadata", label: "Metadata", description: "Title, description, OG tags, etc." },
];

function SwitchRow({
  id,
  label,
  description,
  checked,
  onCheckedChange,
}: {
  id: string;
  label: string;
  description?: string;
  checked: boolean;
  onCheckedChange: (v: boolean) => void;
}) {
  return (
    <div className="flex items-center justify-between">
      <div>
        <Label htmlFor={id} className="text-sm font-medium">
          {label}
        </Label>
        {description && (
          <p className="text-xs text-muted-foreground">{description}</p>
        )}
      </div>
      <Switch id={id} checked={checked} onCheckedChange={onCheckedChange} />
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

export function ScrapeOptions({ state, onChange, source }: ScrapeOptionsProps) {
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

  return (
    <div className="space-y-5">
      <div className="space-y-3">
        <Label className="text-xs text-muted-foreground uppercase tracking-wide">
          Output Formats
        </Label>
        <div className="space-y-1">
          {formatOptions.map(({ value, label, description }) => (
            <SwitchRow
              key={value}
              id={`fmt-${value}`}
              label={label}
              description={description}
              checked={state.formats.includes(value)}
              onCheckedChange={() => toggle(value)}
            />
          ))}
        </div>
      </div>

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
