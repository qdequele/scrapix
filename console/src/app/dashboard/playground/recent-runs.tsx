"use client";

import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { History } from "lucide-react";
import { formatDistanceToNow } from "date-fns";

export interface RunEntry {
  id: string;
  type: "scrape" | "crawl" | "map" | "search";
  url: string;
  status_code?: number;
  duration_ms?: number;
  total_links?: number;
  timestamp: string;
}

const STORAGE_KEY = "scrapix-playground-runs";
const MAX_RUNS = 10;

export function loadRuns(): RunEntry[] {
  if (typeof window === "undefined") return [];
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw ? JSON.parse(raw) : [];
  } catch {
    return [];
  }
}

const MAX_URL_LENGTH = 2048;

/**
 * Keep only the summary fields of a run. History never stores response
 * bodies (markdown, HTML, base64 screenshots): an entry is a few hundred
 * bytes, so 10 of them stay far below the localStorage quota.
 */
function summarize(run: RunEntry): RunEntry {
  return {
    id: run.id,
    type: run.type,
    // Entries read back from storage are untrusted: tolerate a missing url.
    url: typeof run.url === "string" ? run.url.slice(0, MAX_URL_LENGTH) : "",
    status_code: run.status_code,
    duration_ms: run.duration_ms,
    total_links: run.total_links,
    timestamp: run.timestamp,
  };
}

export function saveRun(run: RunEntry): RunEntry[] {
  const runs = [summarize(run), ...loadRuns().map(summarize)].slice(0, MAX_RUNS);
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(runs));
  } catch {
    // Storage full or unavailable (private mode): keep the in-memory list.
  }
  return runs;
}

interface HistoryPanelProps {
  runs: RunEntry[];
  onReplay: (run: RunEntry) => void;
  typeFilter?: "scrape" | "crawl" | "map" | "search";
}

export function HistoryPanel({ runs, onReplay, typeFilter }: HistoryPanelProps) {
  const filteredRuns = typeFilter
    ? runs.filter((r) => r.type === typeFilter)
    : runs;
  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center gap-2 pb-3">
        <span className="text-sm font-medium">History</span>
        {filteredRuns.length > 0 && (
          <Badge variant="secondary" className="text-xs">
            {filteredRuns.length}
          </Badge>
        )}
      </div>

      {filteredRuns.length === 0 ? (
        <div className="flex flex-col items-center justify-center flex-1 text-muted-foreground gap-2 py-10">
          <History className="h-8 w-8 opacity-30" />
          <p className="text-xs">No runs yet</p>
        </div>
      ) : (
        <ScrollArea className="flex-1">
          <div className="space-y-1 pr-2">
            {filteredRuns.map((run) => (
              <button
                key={run.id}
                type="button"
                onClick={() => onReplay(run)}
                className="w-full text-left rounded-md px-2.5 py-2 hover:bg-muted/50 transition-colors cursor-pointer"
              >
                <div className="flex items-center gap-1.5 mb-1">
                  <Badge
                    variant={run.type === "scrape" ? "default" : "secondary"}
                    className="text-[10px] px-1.5 py-0"
                  >
                    {run.type}
                  </Badge>
                  {run.status_code && (
                    <Badge
                      variant={
                        run.status_code >= 200 && run.status_code < 400
                          ? "outline"
                          : "destructive"
                      }
                      className="text-[10px] px-1.5 py-0"
                    >
                      {run.status_code}
                    </Badge>
                  )}
                  {run.total_links != null && (
                    <Badge variant="outline" className="text-[10px] px-1.5 py-0">
                      {run.total_links} links
                    </Badge>
                  )}
                </div>
                <p className="text-xs font-mono text-muted-foreground truncate">
                  {run.url}
                </p>
                <div className="flex items-center gap-2 mt-1 text-[10px] text-muted-foreground">
                  {run.duration_ms != null && (
                    <span>{run.duration_ms}ms</span>
                  )}
                  <span>
                    {formatDistanceToNow(new Date(run.timestamp), {
                      addSuffix: true,
                    })}
                  </span>
                </div>
              </button>
            ))}
          </div>
        </ScrollArea>
      )}
    </div>
  );
}
