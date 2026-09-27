"use client";

import { useState, useCallback, useEffect } from "react";
import { Card, CardContent } from "@/components/ui/card";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { FileUp, Link2 } from "lucide-react";
import { toast } from "sonner";
import { submitParse, submitScrape } from "@/lib/api";
import type { ParserOptions, ScrapeResult } from "@/lib/api-types";
import { UrlBar } from "../playground/url-bar";
import { FileDrop } from "../playground/file-drop";
import {
  DOCUMENT_FORMATS,
  ScrapeOptions,
  type ScrapeSource,
  type ScrapeState,
} from "../playground/scrape-options";
import { ResultPanel } from "../playground/result-panel";
import { HistoryPanel, loadRuns, saveRun, type RunEntry } from "../playground/recent-runs";

const UPLOAD_PREFIX = "upload://";

/** `parsers` for the request, leaving out defaults so the server's apply. */
function parserOptions(state: ScrapeState): ParserOptions | undefined {
  const parsers: ParserOptions = {};
  if (state.ocr_mode !== "off") {
    parsers.ocr = state.ocr_mode;
    const cap = parseInt(state.ocr_max_pages, 10);
    if (cap > 0) parsers.ocr_max_pages = cap;
  }
  const maxPages = parseInt(state.max_pages, 10);
  if (maxPages > 0) parsers.max_pages = maxPages;
  return Object.keys(parsers).length > 0 ? parsers : undefined;
}

export default function ScrapePage() {
  const [source, setSource] = useState<ScrapeSource>("url");
  const [file, setFile] = useState<File | null>(null);
  const [url, setUrl] = useState("https://scrapix.meilisearch.dev");
  const [loading, setLoading] = useState(false);
  const [result, setResult] = useState<ScrapeResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [runs, setRuns] = useState<RunEntry[]>([]);
  const [scrapeState, setScrapeState] = useState<ScrapeState>({
    formats: ["markdown", "metadata"],
    only_main_content: true,
    include_links: false,
    timeout_ms: "30000",
    ai_summary: false,
    feat_schema: false,
    feat_block_split: false,
    feat_custom_selectors: false,
    custom_selectors: "",
    feat_ai_extraction: false,
    ai_extraction_prompt: "",
    ocr_mode: "off",
    ocr_max_pages: "",
    max_pages: "",
  });

  useEffect(() => {
    setRuns(loadRuns());
  }, []);

  const handleScrape = useCallback(async () => {
    if (!url.trim()) {
      toast.error("Please enter a URL");
      return;
    }
    if (scrapeState.formats.length === 0) {
      toast.error("Select at least one output format");
      return;
    }

    setLoading(true);
    setResult(null);
    setError(null);

    try {
      // Build formats list, adding schema/blocks if their features are enabled
      const formats = [...scrapeState.formats];
      if (scrapeState.feat_schema && !formats.includes("schema")) {
        formats.push("schema");
      }
      if (scrapeState.feat_block_split && !formats.includes("blocks")) {
        formats.push("blocks");
      }

      // Build custom CSS selector extract map
      let extract: Record<string, string> | undefined;
      if (scrapeState.feat_custom_selectors && scrapeState.custom_selectors.trim()) {
        try {
          extract = JSON.parse(scrapeState.custom_selectors);
        } catch {
          // ignore invalid JSON
        }
      }

      // Build AI options
      let ai: { summary?: boolean; extract?: { prompt: string } } | undefined;
      if (scrapeState.ai_summary || scrapeState.feat_ai_extraction) {
        ai = {};
        if (scrapeState.ai_summary) ai.summary = true;
        if (scrapeState.feat_ai_extraction && scrapeState.ai_extraction_prompt.trim()) {
          ai.extract = { prompt: scrapeState.ai_extraction_prompt.trim() };
        }
      }

      const data = await submitScrape({
        url,
        formats,
        only_main_content: scrapeState.only_main_content,
        include_links: scrapeState.include_links,
        timeout_ms: parseInt(scrapeState.timeout_ms) || 30000,
        extract,
        ai,
        parsers: parserOptions(scrapeState),
      });
      setResult(data);
      const newRuns = saveRun({
        id: Math.random().toString(36).slice(2) + Date.now().toString(36),
        type: "scrape",
        url,
        status_code: data.status_code,
        duration_ms: data.scrape_duration_ms,
        timestamp: new Date().toISOString(),
      });
      setRuns(newRuns);
    } catch (err) {
      const msg =
        err instanceof Error
          ? err.message
          : "Failed to fetch. Is the API running?";
      setError(msg);
    }

    setLoading(false);
  }, [url, scrapeState]);

  const handleParse = useCallback(async () => {
    if (!file) {
      toast.error("Choose a file to parse");
      return;
    }
    const formats = scrapeState.formats.filter((f) => DOCUMENT_FORMATS.includes(f));
    if (formats.length === 0) {
      toast.error("Select at least one of Markdown, Content, Links or Metadata");
      return;
    }

    setLoading(true);
    setResult(null);
    setError(null);

    let ai: { summary?: boolean; extract?: { prompt: string } } | undefined;
    if (scrapeState.ai_summary || scrapeState.feat_ai_extraction) {
      ai = {};
      if (scrapeState.ai_summary) ai.summary = true;
      if (scrapeState.feat_ai_extraction && scrapeState.ai_extraction_prompt.trim()) {
        ai.extract = { prompt: scrapeState.ai_extraction_prompt.trim() };
      }
    }

    try {
      const data = await submitParse(file, {
        formats,
        include_links: scrapeState.include_links,
        parsers: parserOptions(scrapeState),
        ai,
      });
      setResult(data);
      setRuns(
        saveRun({
          id: Math.random().toString(36).slice(2) + Date.now().toString(36),
          type: "scrape",
          url: `${UPLOAD_PREFIX}${file.name}`,
          status_code: data.status_code,
          duration_ms: data.scrape_duration_ms,
          timestamp: new Date().toISOString(),
        }),
      );
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to parse the file.");
    }

    setLoading(false);
  }, [file, scrapeState]);

  const handleReplay = (run: RunEntry) => {
    if (run.url.startsWith(UPLOAD_PREFIX)) {
      // The file itself is not kept: switch to upload mode to pick it again.
      setSource("file");
      toast.info(`Choose ${run.url.slice(UPLOAD_PREFIX.length)} again to re-parse it`);
      return;
    }
    setSource("url");
    setUrl(run.url);
  };

  const history = (
    <div className="p-3 h-full">
      <HistoryPanel runs={runs} onReplay={handleReplay} typeFilter="scrape" />
    </div>
  );

  return (
    <div className="flex flex-col gap-4 h-full">
      <ToggleGroup
        type="single"
        variant="outline"
        size="sm"
        value={source}
        onValueChange={(v) => {
          if (v) setSource(v as ScrapeSource);
        }}
        className="self-start"
      >
        <ToggleGroupItem
          value="url"
          className="gap-1.5 px-3 data-[state=on]:bg-primary/10 data-[state=on]:text-primary data-[state=on]:border-primary/30"
        >
          <Link2 className="h-3.5 w-3.5" />
          URL
        </ToggleGroupItem>
        <ToggleGroupItem
          value="file"
          className="gap-1.5 px-3 data-[state=on]:bg-primary/10 data-[state=on]:text-primary data-[state=on]:border-primary/30"
        >
          <FileUp className="h-3.5 w-3.5" />
          Upload file
        </ToggleGroupItem>
      </ToggleGroup>

      {source === "url" ? (
        <UrlBar
          mode="scrape"
          url={url}
          onUrlChange={setUrl}
          onSubmit={handleScrape}
          loading={loading}
          historySlot={history}
        />
      ) : (
        <FileDrop
          file={file}
          onFileChange={setFile}
          onSubmit={handleParse}
          loading={loading}
        />
      )}

      <div className="grid grid-cols-1 lg:grid-cols-[minmax(360px,1fr)_3fr] gap-4 flex-1 min-h-0">
        <Card className="overflow-auto">
          <CardContent className="p-4">
            <ScrapeOptions
              state={scrapeState}
              onChange={setScrapeState}
              source={source}
            />
          </CardContent>
        </Card>

        <Card className="overflow-hidden">
          <CardContent className="p-4 h-full">
            <ResultPanel
              result={result}
              crawlResult={null}
              mode={source === "file" ? "parse" : "scrape"}
              loading={loading}
              error={error}
            />
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
