<script lang="ts">
  import { probeMcpWanAccess, type WanProbeResult } from "$lib/api/health";

  interface Props {
    workspaceId: string;
  }

  let { workspaceId }: Props = $props();

  let busy = $state(false);
  let error = $state("");
  let result = $state<WanProbeResult | null>(null);

  $effect(() => {
    workspaceId;
    result = null;
    error = "";
  });

  async function runProbe() {
    if (busy || !workspaceId) return;
    busy = true;
    error = "";
    try {
      result = await probeMcpWanAccess(workspaceId);
    } catch (err) {
      error = String(err);
      result = null;
    } finally {
      busy = false;
    }
  }

  function verdictLabel(verdict: string): string {
    switch (verdict) {
      case "reachable":
        return "外网可达";
      case "host_only":
        return "主机可达";
      case "unreachable":
        return "外网不可达";
      case "inconclusive":
        return "无法确认";
      case "not_configured":
        return "未配置";
      default:
        return verdict;
    }
  }
</script>

<section class="tx-card p-5">
  <div class="flex items-start justify-between gap-3">
    <div>
      <h3 class="font-semibold">外网可达性</h3>
      <p class="mt-1 text-sm text-[var(--color-text-muted)]">
        从公网探测点访问 MCP，避免本机访问公网 URL 的 NAT 回环误报。探测会把公网地址发给第三方节点，不携带 Token。
      </p>
    </div>
    <button
      type="button"
      class="tx-btn-primary shrink-0 disabled:opacity-50"
      disabled={busy}
      onclick={runProbe}
    >
      {busy ? "探测中…" : "从外网探测"}
    </button>
  </div>

  {#if error}
    <p class="mt-4 rounded-lg border border-[var(--color-error)]/30 bg-[var(--color-error)]/10 px-3 py-2 text-sm text-[var(--color-error)]">
      {error}
    </p>
  {/if}

  {#if result}
    <div class="mt-4 rounded-lg bg-[var(--color-bg)] px-3 py-3">
      <div class="flex items-start justify-between gap-3">
        <div class="min-w-0">
          <p class="text-sm font-medium">{result.summary}</p>
          <p class="mt-1 break-all font-mono text-xs text-[var(--color-text-muted)]">
            {result.endpoint || result.publicUrl || "（无公网 URL）"}
          </p>
          {#if result.hint}
            <p class="mt-2 text-xs text-[var(--color-accent)]">{result.hint}</p>
          {/if}
        </div>
        <span
          class="shrink-0 rounded-sm px-2 py-0.5 text-xs font-medium"
          class:health-ok={result.verdict === "reachable"}
          class:health-warn={result.verdict === "host_only" || result.verdict === "inconclusive"}
          class:health-fail={result.verdict === "unreachable" || result.verdict === "not_configured"}
        >
          {verdictLabel(result.verdict)}
        </span>
      </div>
    </div>
    {#if result.steps.length > 0}
      <ul class="mt-2 grid gap-2">
        {#each result.steps as step (step.id)}
          <li
            class="flex items-start justify-between gap-3 rounded-lg bg-[var(--color-bg)] px-3 py-2"
          >
            <div class="min-w-0">
              <p class="text-sm font-medium">{step.label}</p>
              <p class="mt-0.5 text-xs text-[var(--color-text-muted)]">{step.detail}</p>
            </div>
            <span
              class="shrink-0 rounded-sm px-2 py-0.5 text-xs font-medium"
              class:health-ok={step.ok}
              class:health-fail={!step.ok}
            >
              {step.ok ? "通过" : "失败"}
            </span>
          </li>
        {/each}
      </ul>
    {/if}
  {:else if !busy && !error}
    <p class="mt-4 text-sm text-[var(--color-text-muted)]">尚未从外网探测。</p>
  {/if}
</section>

<style>
  .health-ok {
    background: color-mix(in oklch, var(--color-success) 15%, transparent);
    color: var(--color-success);
  }

  .health-fail {
    background: color-mix(in oklch, var(--color-error) 15%, transparent);
    color: var(--color-error);
  }

  .health-warn {
    background: color-mix(in oklch, var(--color-warning, var(--color-accent)) 15%, transparent);
    color: var(--color-warning, var(--color-accent));
  }
</style>
