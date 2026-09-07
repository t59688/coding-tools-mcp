import { invoke } from "@tauri-apps/api/core";

export interface HealthItem {
  label: string;
  ok: boolean;
  detail: string;
  hint: string;
}

export interface WanProbeStep {
  id: string;
  label: string;
  ok: boolean;
  detail: string;
}

export interface WanProbeResult {
  publicUrl: string;
  endpoint: string;
  verdict: "reachable" | "host_only" | "unreachable" | "inconclusive" | "not_configured" | string;
  reachable: boolean;
  summary: string;
  hint: string;
  steps: WanProbeStep[];
}

export async function runHealthChecks(workspaceId: string): Promise<HealthItem[]> {
  return invoke<HealthItem[]>("run_health_checks", { id: workspaceId });
}

export async function probeMcpWanAccess(workspaceId: string): Promise<WanProbeResult> {
  return invoke<WanProbeResult>("probe_mcp_wan_access", { id: workspaceId });
}
