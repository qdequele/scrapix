"use client";

import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import {
  Copy,
  ExternalLink,
  Globe,
  CheckCircle2,
  XCircle,
  ArrowRight,
  Loader2,
  FileText,
  AlertCircle,
  Sparkles,
  Download,
  MousePointerClick,
} from "lucide-react";
import { toast } from "sonner";
import { useEffect, useState, useCallback } from "react";
import Link from "next/link";
import Image from "next/image";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { codeToHtml } from "shiki";
import { HighlightedJson } from "@/components/highlighted-json";
import type { ScrapeResult, Job, ActionErrorDetails } from "@/lib/api-types";
import { fetchJobStatus } from "@/lib/api";

interface ResultPanelProps {
  result: ScrapeResult | null;
  crawlResult: { job_id: string; status: string; message?: string } | null;
  mode: "scrape" | "crawl";
  loading: boolean;
  error: string | null;
  /** Set when the scrape failed with a 422 `action_error` */
  actionError?: ActionErrorDetails | null;
}

const SCRAPE_EXAMPLE = `curl -X POST https://scrapix.meilisearch.dev/scrape \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer YOUR_API_KEY" \\
  -d '{
    "url": "https://example.com",
    "formats": ["markdown", "metadata"],
    "only_main_content": true,
    "timeout_ms": 30000
  }'`;

const CRAWL_EXAMPLE = `curl -X POST https://scrapix.meilisearch.dev/crawl \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer YOUR_API_KEY" \\
  -d '{
    "start_urls": ["https://example.com"],
    "index_uid": "my-index",
    "max_depth": 3,
    "max_pages": 100,
    "meilisearch": {
      "url": "https://ms.example.com",
      "api_key": "YOUR_MEILI_KEY"
    }
  }'`;

export function ResultPanel({
  result,
  crawlResult,
  mode,
  loading,
  error,
  actionError,
}: ResultPanelProps) {
  if (loading) {
    return <LoadingState />;
  }

  if (actionError) {
    return <ActionErrorState details={actionError} />;
  }

  if (error) {
    return <ErrorState error={error} />;
  }

  if (mode === "crawl" && crawlResult) {
    return <CrawlResultState result={crawlResult} />;
  }

  if (mode === "scrape" && result) {
    return <ScrapeResultState result={result} />;
  }

  return <EmptyState mode={mode} />;
}

function EmptyState({ mode }: { mode: "scrape" | "crawl" }) {
  const example = mode === "crawl" ? CRAWL_EXAMPLE : SCRAPE_EXAMPLE;

  return (
    <div className="flex flex-col h-full">
      <div className="px-4 pt-4 pb-2">
        <p className="text-xs text-muted-foreground uppercase tracking-wide font-medium">
          API Example
        </p>
      </div>
      <div className="px-4 pb-4 flex-1 min-h-0">
        <CodeBlock code={example} lang="bash" />
      </div>
    </div>
  );
}

function LoadingState() {
  return (
    <div className="space-y-4 p-4">
      <div className="flex items-center gap-3">
        <Skeleton className="h-6 w-16 rounded-full" />
        <Skeleton className="h-4 w-24" />
        <Skeleton className="h-6 w-12 rounded-full" />
      </div>
      <Skeleton className="h-8 w-full" />
      <div className="space-y-2 pt-2">
        <Skeleton className="h-4 w-full" />
        <Skeleton className="h-4 w-[90%]" />
        <Skeleton className="h-4 w-[95%]" />
        <Skeleton className="h-4 w-[80%]" />
        <Skeleton className="h-4 w-full" />
        <Skeleton className="h-4 w-[85%]" />
        <Skeleton className="h-4 w-[70%]" />
        <Skeleton className="h-4 w-[92%]" />
      </div>
    </div>
  );
}

function ErrorState({ error }: { error: string }) {
  return (
    <div className="flex flex-col items-center justify-center h-full gap-3 py-20">
      <XCircle className="h-10 w-10 text-destructive opacity-60" />
      <p className="text-sm text-destructive font-medium">Request failed</p>
      <p className="text-xs text-muted-foreground max-w-md text-center">
        {error}
      </p>
    </div>
  );
}

function ActionErrorState({ details }: { details: ActionErrorDetails }) {
  return (
    <div className="flex flex-col items-center justify-center h-full gap-3 py-20">
      <MousePointerClick className="h-10 w-10 text-destructive opacity-60" />
      <p className="text-sm text-destructive font-medium">
        Action #{details.action_index + 1} failed
      </p>
      <div className="flex items-center gap-2">
        <Badge variant="outline" className="font-mono text-xs">
          actions[{details.action_index}]
        </Badge>
        <Badge variant="destructive" className="font-mono text-xs">
          {details.action_type}
        </Badge>
      </div>
      <p className="text-xs text-muted-foreground max-w-md text-center break-words">
        {details.message}
      </p>
      <p className="text-[11px] text-muted-foreground max-w-md text-center">
        Nothing was captured and the request was not billed. Fix this action
        and run the scrape again.
      </p>
    </div>
  );
}

/** Approximate decoded size of a base64 string, in KB. */
function base64Kb(b64: string): number {
  return Math.round((b64.length * 3) / 4 / 1024);
}

function screenshotFilename(url: string): string {
  try {
    const host = new URL(url).hostname.replace(/[^a-z0-9.-]/gi, "_");
    return `screenshot-${host}.png`;
  } catch {
    return "screenshot.png";
  }
}

/** The result with its base64 screenshot elided (too large to highlight). */
function withoutScreenshot(result: ScrapeResult): ScrapeResult {
  if (!result.screenshot) return result;
  return {
    ...result,
    screenshot: `<base64 PNG, ${base64Kb(result.screenshot)} KB, see the Screenshot tab>`,
  };
}

export function ScreenshotView({
  screenshot,
  url,
}: {
  screenshot: string;
  url: string;
}) {
  const dataUrl = `data:image/png;base64,${screenshot}`;
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-2">
        <span className="text-xs text-muted-foreground">
          PNG · {base64Kb(screenshot)} KB
        </span>
        <Button variant="outline" size="sm" className="h-7 text-xs" asChild>
          <a href={dataUrl} download={screenshotFilename(url)}>
            <Download className="mr-1.5 h-3 w-3" />
            Download
          </a>
        </Button>
      </div>
      <Image
        src={dataUrl}
        alt={`Screenshot of ${url}`}
        width={0}
        height={0}
        sizes="100vw"
        unoptimized
        className="h-auto w-full rounded-md border"
      />
    </div>
  );
}

function CrawlResultState({
  result,
}: {
  result: { job_id: string; status: string; message?: string };
}) {
  const [jobStatus, setJobStatus] = useState<Job | null>(null);

  const poll = useCallback(() => {
    fetchJobStatus(result.job_id)
      .then(setJobStatus)
      .catch(() => {});
  }, [result.job_id]);

  useEffect(() => {
    poll();
    const interval = setInterval(poll, 2000);
    return () => clearInterval(interval);
  }, [poll]);

  const isRunning =
    jobStatus?.status === "running" || jobStatus?.status === "pending";
  const isCompleted = jobStatus?.status === "completed";
  const isFailed = jobStatus?.status === "failed";

  return (
    <div className="flex flex-col items-center justify-center h-full gap-6">
      {/* Status icon */}
      <div>
        {isRunning && (
          <Loader2 className="h-10 w-10 text-primary animate-spin" />
        )}
        {isCompleted && (
          <CheckCircle2 className="h-10 w-10 text-green-500" />
        )}
        {isFailed && (
          <XCircle className="h-10 w-10 text-destructive" />
        )}
        {!jobStatus && (
          <Globe className="h-10 w-10 text-primary opacity-70" />
        )}
      </div>

      {/* Title + badge */}
      <div className="flex flex-col items-center gap-2">
        <p className="text-lg font-semibold">
          {isRunning
            ? "Crawl in progress..."
            : isCompleted
              ? "Crawl completed"
              : isFailed
                ? "Crawl failed"
                : "Crawl job created"}
        </p>
        <Badge
          variant={isRunning ? "secondary" : isCompleted ? "default" : isFailed ? "destructive" : "outline"}
        >
          {isRunning && (
            <span className="relative mr-1.5 flex h-2 w-2">
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-current opacity-75" />
              <span className="relative inline-flex h-2 w-2 rounded-full bg-current" />
            </span>
          )}
          {jobStatus?.status ?? result.status}
        </Badge>
      </div>

      {/* Counters */}
      {jobStatus && (
        <div className="flex items-center gap-6 text-sm text-muted-foreground">
          <span className="flex items-center gap-1.5">
            <FileText className="h-4 w-4" />
            <span className="font-medium text-foreground">{jobStatus.pages_crawled}</span> crawled
          </span>
          <span className="flex items-center gap-1.5">
            <CheckCircle2 className="h-4 w-4" />
            <span className="font-medium text-foreground">{jobStatus.pages_indexed}</span> indexed
          </span>
          <span className={cn("flex items-center gap-1.5", jobStatus.errors > 0 && "text-destructive")}>
            <AlertCircle className="h-4 w-4" />
            <span className="font-medium">{jobStatus.errors}</span> errors
          </span>
        </div>
      )}

      {/* Job ID */}
      <p className="text-xs text-muted-foreground font-mono">
        {result.job_id}
      </p>

      {/* Subtitle */}
      <p className="text-sm text-muted-foreground text-center max-w-xs">
        {isRunning
          ? "Pages will appear once the crawl finishes indexing."
          : isCompleted
            ? "Crawl complete. View full details to explore results."
            : isFailed
              ? "Check the job details for error information."
              : "Crawl job has been submitted."}
      </p>

      {/* Details link */}
      <Button variant="outline" asChild>
        <Link href={`/dashboard/jobs/${result.job_id}`}>
          View details
          <ArrowRight className="ml-1.5 h-4 w-4" />
        </Link>
      </Button>
    </div>
  );
}

function ScrapeResultState({ result }: { result: ScrapeResult }) {
  const availableTabs: { value: string; label: string }[] = [];
  if (result.screenshot) availableTabs.push({ value: "screenshot", label: "Screenshot" });
  if (result.markdown) availableTabs.push({ value: "markdown", label: "Markdown" });
  if (result.metadata) availableTabs.push({ value: "metadata", label: "Metadata" });
  if (result.links && result.links.length > 0)
    availableTabs.push({ value: "links", label: "Links" });
  if (result.html) availableTabs.push({ value: "html", label: "HTML" });
  if (result.raw_html) availableTabs.push({ value: "rawhtml", label: "Raw HTML" });
  if (result.content) availableTabs.push({ value: "content", label: "Content" });
  if (result.schema && Object.keys(result.schema).length > 0)
    availableTabs.push({ value: "schema", label: "Schema" });
  if (result.blocks && result.blocks.length > 0)
    availableTabs.push({ value: "blocks", label: "Blocks" });
  if (result.extract && Object.keys(result.extract).length > 0)
    availableTabs.push({ value: "extract", label: "Extract" });
  if (result.ai?.summary) availableTabs.push({ value: "ai-summary", label: "AI Summary" });
  if (result.ai?.extract) availableTabs.push({ value: "ai-extract", label: "AI Extract" });
  if (result.actions) availableTabs.push({ value: "actions", label: "Actions" });
  availableTabs.push({ value: "json", label: "JSON" });

  const defaultTab = availableTabs[0]?.value ?? "json";

  const isSuccess = result.status_code >= 200 && result.status_code < 400;

  const copyText = (text: string) => {
    navigator.clipboard.writeText(text);
    toast.success("Copied to clipboard");
  };

  return (
    <div className="flex flex-col h-full">
      {/* Warning */}
      {result.warning && (
        <div className="flex items-center gap-2 px-3 py-2 mb-2 rounded-md bg-yellow-500/10 text-yellow-700 dark:text-yellow-400 text-xs">
          <AlertCircle className="h-3.5 w-3.5 shrink-0" />
          {result.warning}
        </div>
      )}

      {/* Header row */}
      <div className="flex items-center gap-2 px-1 pb-3 flex-wrap">
        <Badge variant={isSuccess ? "default" : "destructive"}>
          {result.status_code}
        </Badge>
        <span className="text-xs text-muted-foreground">
          {result.scrape_duration_ms}ms
        </span>
        {result.language && (
          <Badge variant="outline" className="text-xs">
            {result.language}
          </Badge>
        )}
        <div className="ml-auto flex items-center gap-1">
          <Button
            variant="ghost"
            size="icon"
            className="h-7 w-7"
            onClick={() => copyText(JSON.stringify(result, null, 2))}
          >
            <Copy className="h-3.5 w-3.5" />
          </Button>
          <Button
            variant="ghost"
            size="icon"
            className="h-7 w-7"
            asChild
          >
            <a href={result.url} target="_blank" rel="noopener noreferrer">
              <ExternalLink className="h-3.5 w-3.5" />
            </a>
          </Button>
        </div>
      </div>

      {/* Tabs */}
      <Tabs defaultValue={defaultTab} className="flex-1 flex flex-col min-h-0">
        <TabsList className="w-full justify-start flex-wrap h-auto gap-1">
          {availableTabs.map((tab) => (
            <TabsTrigger
              key={tab.value}
              value={tab.value}
              className="text-xs"
            >
              {tab.label}
            </TabsTrigger>
          ))}
        </TabsList>

        <div className="flex-1 min-h-0 border rounded-md">
          {result.screenshot && (
            <TabsContent value="screenshot" className="h-full m-0">
              <ScrollArea className="h-full">
                <div className="p-4">
                  <ScreenshotView screenshot={result.screenshot} url={result.url} />
                </div>
              </ScrollArea>
            </TabsContent>
          )}

          {result.markdown && (
            <TabsContent value="markdown" className="h-full m-0">
              <ScrollArea className="h-full">
                <div className="prose prose-sm dark:prose-invert max-w-none p-4">
                  <ReactMarkdown remarkPlugins={[remarkGfm]}>
                    {result.markdown}
                  </ReactMarkdown>
                </div>
              </ScrollArea>
            </TabsContent>
          )}

          {result.metadata && (
            <TabsContent value="metadata" className="h-full m-0">
              <ScrollArea className="h-full">
                <MetadataView metadata={result.metadata} />
              </ScrollArea>
            </TabsContent>
          )}

          {result.links && result.links.length > 0 && (
            <TabsContent value="links" className="h-full m-0">
              <ScrollArea className="h-full">
                <div className="p-4 space-y-1">
                  {result.links.map((link, i) => (
                    <a
                      key={i}
                      href={link}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="flex items-center gap-2 text-sm font-mono text-muted-foreground hover:text-foreground transition-colors py-0.5"
                    >
                      <ExternalLink className="h-3 w-3 shrink-0" />
                      <span className="truncate">{link}</span>
                    </a>
                  ))}
                </div>
              </ScrollArea>
            </TabsContent>
          )}

          {result.html && (
            <TabsContent value="html" className="h-full m-0">
              <ScrollArea className="h-full">
                <pre className="whitespace-pre-wrap font-mono text-xs p-4">
                  {result.html}
                </pre>
              </ScrollArea>
            </TabsContent>
          )}

          {result.raw_html && (
            <TabsContent value="rawhtml" className="h-full m-0">
              <ScrollArea className="h-full">
                <pre className="whitespace-pre-wrap font-mono text-xs p-4">
                  {result.raw_html}
                </pre>
              </ScrollArea>
            </TabsContent>
          )}

          {result.content && (
            <TabsContent value="content" className="h-full m-0">
              <ScrollArea className="h-full">
                <pre className="whitespace-pre-wrap font-mono text-sm p-4 leading-relaxed">
                  {result.content}
                </pre>
              </ScrollArea>
            </TabsContent>
          )}

          {result.schema && Object.keys(result.schema).length > 0 && (
            <TabsContent value="schema" className="h-full m-0">
              <ScrollArea className="h-full">
                <HighlightedJson code={JSON.stringify(result.schema, null, 2)} />
              </ScrollArea>
            </TabsContent>
          )}

          {result.blocks && result.blocks.length > 0 && (
            <TabsContent value="blocks" className="h-full m-0">
              <ScrollArea className="h-full">
                <div className="p-4 space-y-4">
                  {result.blocks.map((block, i) => (
                    <div key={i} className="space-y-1">
                      {block.heading && (
                        <p className="text-sm font-semibold">
                          {"#".repeat(block.heading_level ?? 2)} {block.heading}
                        </p>
                      )}
                      <p className="text-xs text-muted-foreground whitespace-pre-wrap">
                        {block.content}
                      </p>
                    </div>
                  ))}
                </div>
              </ScrollArea>
            </TabsContent>
          )}

          {result.extract && Object.keys(result.extract).length > 0 && (
            <TabsContent value="extract" className="h-full m-0">
              <ScrollArea className="h-full">
                <HighlightedJson code={JSON.stringify(result.extract, null, 2)} />
              </ScrollArea>
            </TabsContent>
          )}

          {result.ai?.summary && (
            <TabsContent value="ai-summary" className="h-full m-0">
              <ScrollArea className="h-full">
                <div className="p-4 space-y-3">
                  <div className="flex items-center gap-2 text-xs text-muted-foreground">
                    <Sparkles className="h-3.5 w-3.5" />
                    <span>Generated by Claude Haiku</span>
                  </div>
                  <p className="text-sm leading-relaxed">{result.ai.summary}</p>
                </div>
              </ScrollArea>
            </TabsContent>
          )}

          {result.ai?.extract && (
            <TabsContent value="ai-extract" className="h-full m-0">
              <ScrollArea className="h-full">
                <div className="p-4 space-y-3">
                  <div className="flex items-center gap-2 text-xs text-muted-foreground">
                    <Sparkles className="h-3.5 w-3.5" />
                    <span>AI-extracted structured data</span>
                  </div>
                  <HighlightedJson code={JSON.stringify(result.ai.extract, null, 2)} />
                </div>
              </ScrollArea>
            </TabsContent>
          )}

          {result.actions && (
            <TabsContent value="actions" className="h-full m-0">
              <ScrollArea className="h-full">
                <div className="p-4 space-y-3">
                  <p className="text-xs text-muted-foreground">
                    Values returned by the{" "}
                    <code className="font-mono">execute_javascript</code>{" "}
                    actions, in order.
                  </p>
                  {result.actions.javascript_returns.length === 0 ? (
                    <p className="text-sm text-muted-foreground">
                      All actions ran. None of them was an{" "}
                      <code className="font-mono">execute_javascript</code>{" "}
                      action.
                    </p>
                  ) : (
                    result.actions.javascript_returns.map((value, i) => (
                      <div key={i} className="space-y-1">
                        <Badge variant="outline" className="font-mono text-[10px]">
                          javascript_returns[{i}]
                        </Badge>
                        <div className="rounded-md border">
                          <HighlightedJson
                            code={JSON.stringify(value, null, 2) ?? "null"}
                          />
                        </div>
                      </div>
                    ))
                  )}
                </div>
              </ScrollArea>
            </TabsContent>
          )}

          <TabsContent value="json" className="h-full m-0">
            <ScrollArea className="h-full">
              <HighlightedJson code={JSON.stringify(withoutScreenshot(result), null, 2)} />
            </ScrollArea>
          </TabsContent>
        </div>
      </Tabs>
    </div>
  );
}

function MetadataView({ metadata }: { metadata: NonNullable<ScrapeResult["metadata"]> }) {
  return (
    <div className="p-4 space-y-4">
      {metadata.title && (
        <MetaRow label="Title" value={metadata.title} />
      )}
      {metadata.description && (
        <MetaRow label="Description" value={metadata.description} />
      )}
      {metadata.author && (
        <MetaRow label="Author" value={metadata.author} />
      )}
      {metadata.canonical_url && (
        <MetaRow label="Canonical URL" value={metadata.canonical_url} mono />
      )}
      {metadata.published_date && (
        <MetaRow label="Published" value={metadata.published_date} />
      )}

      {metadata.keywords && metadata.keywords.length > 0 && (
        <div className="space-y-1">
          <p className="text-xs text-muted-foreground uppercase tracking-wide font-medium">
            Keywords
          </p>
          <div className="flex flex-wrap gap-1">
            {metadata.keywords.map((kw, i) => (
              <Badge key={i} variant="secondary" className="text-xs">
                {kw}
              </Badge>
            ))}
          </div>
        </div>
      )}

      {metadata.open_graph &&
        Object.keys(metadata.open_graph).length > 0 && (
          <div className="space-y-2">
            <p className="text-xs text-muted-foreground uppercase tracking-wide font-medium">
              Open Graph
            </p>
            <div className="space-y-1">
              {Object.entries(metadata.open_graph).map(([key, val]) => (
                <MetaRow key={key} label={key} value={val} small />
              ))}
            </div>
          </div>
        )}

      {metadata.twitter &&
        Object.keys(metadata.twitter).length > 0 && (
          <div className="space-y-2">
            <p className="text-xs text-muted-foreground uppercase tracking-wide font-medium">
              Twitter
            </p>
            <div className="space-y-1">
              {Object.entries(metadata.twitter).map(([key, val]) => (
                <MetaRow key={key} label={key} value={val} small />
              ))}
            </div>
          </div>
        )}
    </div>
  );
}

function HighlightedCode({ code, lang = "json" }: { code: string; lang?: string }) {
  const [html, setHtml] = useState<string>("");

  useEffect(() => {
    let cancelled = false;
    codeToHtml(code, {
      lang,
      themes: { light: "github-light", dark: "github-dark" },
      defaultColor: false,
    }).then((result) => {
      if (!cancelled) setHtml(result);
    });
    return () => {
      cancelled = true;
    };
  }, [code, lang]);

  if (!html) {
    return (
      <pre className="whitespace-pre-wrap font-mono text-xs p-4">{code}</pre>
    );
  }

  return (
    <div
      className="p-4 text-xs [&_pre]:!bg-transparent [&_code]:!bg-transparent [&_.shiki]:!bg-transparent"
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}

export function CodeBlock({ code, lang = "bash" }: { code: string; lang?: string }) {
  const [copied, setCopied] = useState(false);

  const handleCopy = () => {
    navigator.clipboard.writeText(code);
    setCopied(true);
    toast.success("Copied to clipboard");
    setTimeout(() => setCopied(false), 2000);
  };

  return (
    <div className="relative rounded-lg border bg-muted/30 overflow-hidden">
      <div className="flex items-center justify-between px-4 py-2 border-b bg-muted/50">
        <span className="text-xs text-muted-foreground font-mono">{lang}</span>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 px-2 text-xs text-muted-foreground hover:text-foreground"
          onClick={handleCopy}
        >
          {copied ? (
            <CheckCircle2 className="mr-1.5 h-3 w-3" />
          ) : (
            <Copy className="mr-1.5 h-3 w-3" />
          )}
          {copied ? "Copied" : "Copy"}
        </Button>
      </div>
      <ScrollArea className="max-h-[500px]">
        <HighlightedCode code={code} lang={lang} />
      </ScrollArea>
    </div>
  );
}


function MetaRow({
  label,
  value,
  mono,
  small,
}: {
  label: string;
  value: string;
  mono?: boolean;
  small?: boolean;
}) {
  return (
    <div className={small ? "flex gap-2 items-baseline" : "space-y-0.5"}>
      <p
        className={cn(
          "text-xs text-muted-foreground",
          small ? "shrink-0 w-24" : "uppercase tracking-wide font-medium"
        )}
      >
        {label}
      </p>
      <p
        className={cn(
          "text-sm",
          mono && "font-mono",
          small && "text-xs truncate"
        )}
      >
        {value}
      </p>
    </div>
  );
}
