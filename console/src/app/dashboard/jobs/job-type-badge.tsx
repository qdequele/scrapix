import { Files, Layers, Sparkles, type LucideIcon } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import type { JobType } from "@/lib/api-types";

const JOB_TYPES: Record<JobType, { label: string; icon: LucideIcon }> = {
  crawl: { label: "Crawl", icon: Layers },
  batch_scrape: { label: "Batch scrape", icon: Files },
  extract: { label: "Extract", icon: Sparkles },
};

/** Engines that predate `job_type` only ran crawls. */
function jobTypeInfo(type: JobType | undefined) {
  return (type && JOB_TYPES[type]) || JOB_TYPES.crawl;
}

export function jobTypeLabel(type: JobType | undefined): string {
  return jobTypeInfo(type).label;
}

/** The kind of job: crawl, batch scrape or extract. */
export function JobTypeBadge({
  type,
  className,
}: {
  type: JobType | undefined;
  className?: string;
}) {
  const { label, icon: Icon } = jobTypeInfo(type);
  return (
    <Badge variant="outline" className={cn("gap-1 font-normal", className)}>
      <Icon className="h-3 w-3" />
      {label}
    </Badge>
  );
}
