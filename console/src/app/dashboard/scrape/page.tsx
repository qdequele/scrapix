"use client";

import { useState, useCallback } from "react";
import { Card, CardContent } from "@/components/ui/card";
import { toast } from "sonner";
import { ApiRequestError, submitScrape, type ScrapeOptions as ScrapeRequestOptions } from "@/lib/api";
import type { ActionErrorDetails, ScrapeResult } from "@/lib/api-types";
import { UrlBar } from "../playground/url-bar";
import {
  ScrapeOptions,
  buildCookies,
  cookieRowError,
  parseActions,
  type ScrapeState,
} from "../playground/scrape-options";
import { ResultPanel } from "../playground/result-panel";
import { HistoryPanel, loadRuns, saveRun, type RunEntry } from "../playground/recent-runs";

function isActionErrorDetails(value: unknown): value is ActionErrorDetails {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v.action_index === "number" &&
    typeof v.action_type === "string" &&
    typeof v.message === "string"
  );
}

export default function ScrapePage() {
  const [url, setUrl] = useState("https://scrapix.meilisearch.dev");
  const [loading, setLoading] = useState(false);
  const [result, setResult] = useState<ScrapeResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<ActionErrorDetails | null>(null);
  // History renders inside a popover (client only), so reading storage here is safe.
  const [runs, setRuns] = useState<RunEntry[]>(loadRuns);
  const [scrapeState, setScrapeState] = useState<ScrapeState>({
    formats: ["markdown", "metadata"],
    only_main_content: true,
    include_links: false,
    timeout_ms: "30000",
    screenshot_full_page: true,
    render_js: false,
    mobile: false,
    feat_actions: false,
    actions_json: "",
    feat_cookies: false,
    cookies: [],
    ai_summary: false,
    feat_schema: false,
    feat_block_split: false,
    feat_custom_selectors: false,
    custom_selectors: "",
    feat_ai_extraction: false,
    ai_extraction_prompt: "",
  });

  const handleScrape = useCallback(async () => {
    if (!url.trim()) {
      toast.error("Please enter a URL");
      return;
    }
    if (scrapeState.formats.length === 0) {
      toast.error("Select at least one output format");
      return;
    }

    // Browser options: validate before sending
    let actions: ScrapeRequestOptions["actions"];
    if (scrapeState.feat_actions && scrapeState.actions_json.trim()) {
      const parsed = parseActions(scrapeState.actions_json);
      if (!parsed.ok) {
        toast.error("Fix the page actions first", { description: parsed.errors[0] });
        return;
      }
      actions = parsed.actions;
    }
    let cookies: ScrapeRequestOptions["cookies"];
    if (scrapeState.feat_cookies) {
      const cookieError = scrapeState.cookies
        .map((row) => cookieRowError(row, url))
        .find((e) => e !== null);
      if (cookieError) {
        toast.error("Fix the cookies first", { description: cookieError });
        return;
      }
      const built = buildCookies(scrapeState.cookies);
      if (built.length > 0) cookies = built;
    }

    setLoading(true);
    setResult(null);
    setError(null);
    setActionError(null);

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
        // Screenshot, actions and mobile make the engine use the browser on
        // its own; only send the explicit choice (keeps its error messages precise).
        render_js: scrapeState.render_js || undefined,
        screenshot: formats.includes("screenshot")
          ? { full_page: scrapeState.screenshot_full_page }
          : undefined,
        mobile: scrapeState.mobile || undefined,
        actions,
        cookies,
      });
      setResult(data);
      // History keeps a summary only: never the response body or screenshot.
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
      if (err instanceof ApiRequestError && err.body) {
        if (err.body.code === "action_error" && isActionErrorDetails(err.body.details)) {
          setActionError(err.body.details);
        }
        setError(err.body.error);
      } else {
        setError(
          err instanceof Error ? err.message : "Failed to fetch. Is the API running?",
        );
      }
    }

    setLoading(false);
  }, [url, scrapeState]);

  const handleReplay = (run: RunEntry) => {
    setUrl(run.url);
  };

  return (
    <div className="flex flex-col gap-4 h-full">
      <UrlBar
        mode="scrape"
        url={url}
        onUrlChange={setUrl}
        onSubmit={handleScrape}
        loading={loading}
        historySlot={
          <div className="p-3 h-full">
            <HistoryPanel runs={runs} onReplay={handleReplay} typeFilter="scrape" />
          </div>
        }
      />

      <div className="grid grid-cols-1 lg:grid-cols-[minmax(360px,1fr)_3fr] gap-4 flex-1 min-h-0">
        <Card className="overflow-auto">
          <CardContent className="p-4">
            <ScrapeOptions state={scrapeState} onChange={setScrapeState} targetUrl={url} />
          </CardContent>
        </Card>

        <Card className="overflow-hidden">
          <CardContent className="p-4 h-full">
            <ResultPanel
              result={result}
              crawlResult={null}
              mode="scrape"
              loading={loading}
              error={error}
              actionError={actionError}
            />
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
