<script lang="ts">
  import { onMount } from "svelte";
  import { formatCallAge, formatCallClock, recentCallLabel, type RecentMcpCall } from "$lib/api/usage";

  interface Props {
    calls?: RecentMcpCall[];
  }

  let { calls = [] }: Props = $props();
  let nowMs = $state(Date.now());

  onMount(() => {
    const timer = window.setInterval(() => {
      nowMs = Date.now();
    }, 1000);
    return () => window.clearInterval(timer);
  });
</script>

<article class="tx-card p-5">
  <div class="mb-3">
    <p class="tx-section-label">最近 MCP 指令</p>
    <p class="mt-1 text-xs text-[var(--color-text-muted)]">当前会话最近 10 条，不含通知。重启应用后清空。</p>
  </div>

  {#if calls.length === 0}
    <p class="text-sm text-[var(--color-text-muted)]">还没有收到 MCP 调用。</p>
  {:else}
    <ol class="tx-recent-call-list">
      {#each calls as call, index ( `${call.atMs}-${call.method}-${call.tool}-${index}` )}
        <li class="tx-recent-call-item">
          <span
            class="tx-recent-call-dot"
            class:error={call.isError}
            aria-label={call.isError ? "失败" : "成功"}
          ></span>
          <div class="min-w-0">
            <p class="tx-mono truncate text-sm">{recentCallLabel(call)}</p>
            {#if call.tool && call.method !== "tools/call"}
              <p class="truncate text-[11px] text-[var(--color-text-muted)]">{call.method}</p>
            {/if}
          </div>
          <span class="tx-recent-call-time">
            <span class="tx-mono">{formatCallClock(call.atMs)}</span>
            <span>{formatCallAge(call.atMs, nowMs)}</span>
          </span>
        </li>
      {/each}
    </ol>
  {/if}
</article>
