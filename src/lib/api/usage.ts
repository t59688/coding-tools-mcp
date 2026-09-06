import { invoke } from "@tauri-apps/api/core";

export type UsageService = "mcp" | "actions";

export interface ServiceUsageStats {
  workspaceId: string;
  service: UsageService;
  requestCount: number;
  toolCallCount: number;
  errorCount: number;
  inputBytes: number;
  outputBytes: number;
  estimatedInputTokens: number;
  estimatedOutputTokens: number;
  estimatedTokens: number;
  lastSuccessAtMs?: number | null;
  lastFailureAtMs?: number | null;
  recentCalls?: RecentMcpCall[];
}

export interface RecentMcpCall {
  atMs: number;
  method: string;
  tool: string;
  isError: boolean;
}

export interface LastCallAges {
  lastSuccessAtMs: number | null;
  lastFailureAtMs: number | null;
}

export function emptyLastCallAges(): LastCallAges {
  return { lastSuccessAtMs: null, lastFailureAtMs: null };
}

export function lastCallAgesFromStats(stats?: ServiceUsageStats): LastCallAges {
  return {
    lastSuccessAtMs: stats?.lastSuccessAtMs ?? null,
    lastFailureAtMs: stats?.lastFailureAtMs ?? null,
  };
}

export function recentCallLabel(call: RecentMcpCall): string {
  return call.tool || call.method || "未知指令";
}

export function formatCallClock(atMs: number | null | undefined): string {
  if (!atMs || atMs <= 0) return "";
  return new Date(atMs).toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

export function formatCallAge(atMs: number | null | undefined, nowMs: number): string {
  if (!atMs || atMs <= 0) return "从未";
  const elapsed = Math.max(0, nowMs - atMs);
  if (elapsed < 1_000) return "刚刚";
  const seconds = Math.floor(elapsed / 1_000);
  if (seconds < 60) return `${seconds} 秒前`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  return `${days} 天前`;
}

export function getServiceUsageStats(id: string): Promise<ServiceUsageStats[]> {
  return invoke<ServiceUsageStats[]>("get_service_usage_stats", { id });
}
