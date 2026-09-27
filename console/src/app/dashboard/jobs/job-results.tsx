"use client";

import { useState } from "react";
import { useInfiniteQuery } from "@tanstack/react-query";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { toast } from "sonner";
import {
  AlertCircle,
  CheckCircle2,
  ChevronDown,
  Copy,
  ExternalLink,
  FileText,
  Loader2,
  RefreshCw,
  XCircle,
} from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import { ApiRequestError, fetchJobResults } from "@/lib/api";
import type { JobResultItem } from "@/lib/api-types";
import { ScreenshotView } from "@/app/dashboard/playground/result-panel";

const TERMINAL = new Set(["completed", "failed", "cancelled"]);
const PAGE_SIZE = 20;
const POLL_MS = 3_000;
const PREVIEW_CHARS = 280;

export function jobResultsQueryKey(jobId: string) {
  return ["job-results", jobId] as const;
}

function errorMessage(err: Error): string {
  if (err instanceof ApiRequestError && err.body) return err.body.error;
  return err.message;
}

function previewText(item: JobResultItem): string | undefined {
  return item.markdown ?? item.content ?? item.ai?.summary;
}

function ResultRow({ item }: { item: JobResultItem }) {
  const [open, setOpen] = useState(false);
  const href = item.block_url ?? item.url;
  const title = item.metadata?.title;
  const description = item.metadata?.description;
  const text = previewText(item);
  const failed = !item.success;
  const expandable = Boolean(text || item.screenshot || item.ai?.extract || item.extract);

  const copyJson = () => {
    navigator.clipboard.writeText(JSON.stringify(item, null, 2));
    toast.success("Copied JSON");
  };

  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <div
        className={cn(
          "rounded-lg border p-3 space-y-1.5",
          failed && "border-destructive/40 bg-destructive/5",
        )}
      >
        <div className="flex items-center gap-2 min-w-0">
          {failed ? (
            <XCircle className="h-4 w-4 shrink-0 text-destructive" />
          ) : (
            <CheckCircle2 className="h-4 w-4 shrink-0 text-emerald-500" />
          )}
          {item.index != null && (
            <Badge variant="outline" className="font-mono text-[10px] px-1.5 py-0 shrink-0">
              #{item.index + 1}
            </Badge>
          )}
          <a
            href={href}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex min-w-0 items-center gap-1 font-mono text-xs hover:underline"
          >
            <span className="truncate">{href}</span>
            <ExternalLink className="h-3 w-3 shrink-0 text-muted-foreground" />
          </a>
          <div className="ml-auto flex shrink-0 items-center gap-1.5">
            {item.status_code != null && (
              <Badge
                variant={item.status_code >= 400 ? "destructive" : "secondary"}
                className="text-[10px] px-1.5 py-0"
              >
                {item.status_code}
              </Badge>
            )}
            {item.scrape_duration_ms != null && (
              <span className="text-[11px] text-muted-foreground">
                {item.scrape_duration_ms}ms
              </span>
            )}
            <Button
              variant="ghost"
              size="icon-xs"
              aria-label="Copy result JSON"
              onClick={copyJson}
            >
              <Copy />
            </Button>
          </div>
        </div>

        {title && <p className="text-sm font-medium leading-snug">{title}</p>}
        {description && (
          <p className="text-xs text-muted-foreground line-clamp-2">{description}</p>
        )}
        {item.source_url && item.source_url !== item.url && (
          <p className="text-[11px] text-muted-foreground font-mono truncate">
            Submitted as {item.source_url}
          </p>
        )}

        {failed && (
          <p className="text-xs text-destructive break-words">
            {item.error ? (
              <>
                <code className="font-mono font-medium">{item.error.code}</code>
                {": "}
                {item.error.message}
              </>
            ) : (
              "Failed"
            )}
          </p>
        )}

        {text && !open && (
          <p className="text-xs text-muted-foreground whitespace-pre-line line-clamp-3">
            {text.slice(0, PREVIEW_CHARS)}
            {text.length > PREVIEW_CHARS ? "…" : ""}
          </p>
        )}

        {expandable && (
          <CollapsibleTrigger asChild>
            <Button variant="ghost" size="xs" className="-ml-2 text-muted-foreground">
              <ChevronDown
                className={cn("transition-transform", open && "rotate-180")}
              />
              {open ? "Show less" : "Show more"}
            </Button>
          </CollapsibleTrigger>
        )}

        <CollapsibleContent className="space-y-3 pt-1">
          {item.screenshot && (
            <ScreenshotView screenshot={item.screenshot} url={item.url} />
          )}
          {item.markdown ? (
            <div className="prose prose-sm dark:prose-invert max-w-none max-h-[480px] overflow-auto rounded-md border p-3">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{item.markdown}</ReactMarkdown>
            </div>
          ) : (
            text && (
              <pre className="whitespace-pre-wrap font-mono text-xs max-h-[480px] overflow-auto rounded-md border p-3">
                {text}
              </pre>
            )
          )}
          {(item.ai?.extract || item.extract) && (
            <pre className="whitespace-pre-wrap font-mono text-xs max-h-[320px] overflow-auto rounded-md border p-3">
              {JSON.stringify(item.ai?.extract ?? item.extract, null, 2)}
            </pre>
          )}
        </CollapsibleContent>
      </div>
    </Collapsible>
  );
}

/**
 * Pages through `GET /job/{id}/results` with the opaque `next` cursor.
 * While the job runs `next` stays set, so the loaded pages are refetched
 * periodically to pick up new results.
 */
export function JobResults({ jobId }: { jobId: string }) {
  const query = useInfiniteQuery({
    queryKey: jobResultsQueryKey(jobId),
    queryFn: ({ pageParam }) => fetchJobResults(jobId, pageParam, PAGE_SIZE),
    initialPageParam: null as string | null,
    getNextPageParam: (lastPage) => lastPage.next,
    refetchInterval: (q) => {
      const pages = q.state.data?.pages;
      const status = pages?.[pages.length - 1]?.status;
      return status && TERMINAL.has(status) ? false : POLL_MS;
    },
  });

  const pages = query.data?.pages ?? [];
  const lastPage = pages[pages.length - 1];
  const items = pages.flatMap((p) => p.data);
  const terminal = lastPage ? TERMINAL.has(lastPage.status) : false;
  // While the job runs, a partial last page fills up on the next refetch;
  // only a full page means there is a further page to request.
  const canLoadMore =
    query.hasNextPage &&
    lastPage !== undefined &&
    (terminal || lastPage.data.length >= PAGE_SIZE);
  const failedCount = items.filter((i) => !i.success).length;

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <FileText className="h-4 w-4 text-muted-foreground" />
        <span className="text-sm font-medium">Results</span>
        {lastPage && (
          <Badge variant="secondary" className="text-xs">
            {lastPage.total} total
          </Badge>
        )}
        {items.length > 0 && lastPage && items.length < lastPage.total && (
          <span className="text-xs text-muted-foreground">
            {items.length} loaded
          </span>
        )}
        {failedCount > 0 && (
          <Badge variant="destructive" className="text-xs">
            {failedCount} failed
          </Badge>
        )}
        {lastPage && !terminal && (
          <span className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <span className="relative flex h-2 w-2">
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-primary opacity-75" />
              <span className="relative inline-flex h-2 w-2 rounded-full bg-primary" />
            </span>
            Live
          </span>
        )}
        <Button
          variant="ghost"
          size="icon-sm"
          className="ml-auto"
          aria-label="Refresh results"
          onClick={() => query.refetch()}
          disabled={query.isFetching}
        >
          <RefreshCw className={cn("h-3.5 w-3.5", query.isFetching && "animate-spin")} />
        </Button>
      </div>

      {query.error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{errorMessage(query.error)}</AlertDescription>
        </Alert>
      )}

      {query.isPending && (
        <div className="space-y-2">
          <Skeleton className="h-16 w-full" />
          <Skeleton className="h-16 w-full" />
          <Skeleton className="h-16 w-full" />
        </div>
      )}

      {!query.isPending && !query.error && items.length === 0 && (
        <div className="flex flex-col items-center justify-center gap-2 py-10 text-muted-foreground">
          {terminal ? (
            <p className="text-sm">This job produced no results.</p>
          ) : (
            <>
              <Loader2 className="h-5 w-5 animate-spin" />
              <p className="text-sm">Waiting for the first results…</p>
            </>
          )}
        </div>
      )}

      <div className="space-y-2">
        {pages.map((page, p) =>
          page.data.map((item, i) => (
            <ResultRow
              key={`${p}-${i}-${item.document_id ?? item.index ?? item.url}`}
              item={item}
            />
          )),
        )}
      </div>

      {canLoadMore && (
        <div className="flex justify-center">
          <Button
            variant="outline"
            size="sm"
            onClick={() => query.fetchNextPage()}
            disabled={query.isFetchingNextPage}
          >
            {query.isFetchingNextPage && <Loader2 className="animate-spin" />}
            Load more
          </Button>
        </div>
      )}
      {!terminal && items.length > 0 && !canLoadMore && (
        <p className="text-center text-xs text-muted-foreground">
          New results appear as the job runs.
        </p>
      )}
    </div>
  );
}
