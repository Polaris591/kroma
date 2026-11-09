<script lang="ts">
  import { BrainCircuit, Terminal, LoaderCircle, ChevronRight, CircleAlert } from '@lucide/svelte';
  import Markdown from './Markdown.svelte';
  import StreamingMarkdown from './StreamingMarkdown.svelte';
  import { latestStreamingText } from '../lib/transcript';
  import { toolLabel, toolLabels } from '../lib/plugin-ui';
  import type { LiveActivity } from '../lib/transcript';
  export let activities: LiveActivity[] = [];
  export let elapsed = 0;
  let logExpanded = false;

  $: active = activities.filter(a => a.kind === 'tool' && !a.done);
  $: finished = activities.filter(a => a.kind === 'tool' && a.done);
  $: failed = finished.filter(a => a.failed).length;
  $: last = activities[activities.length - 1];
  // Keep the newest model text visible even when a tool starts afterwards.
  // Stream through the same sanitized Markdown renderer as stored replies.
  $: latestText = latestStreamingText(activities);
  $: stage = active.length ? '正在执行工具' :
    last?.kind === 'reasoning' ? '正在思考' :
    last?.kind === 'text' ? '正在生成回复' : '正在处理';
  $: duration = `${Math.floor(elapsed / 60).toString().padStart(2, '0')}:${(elapsed % 60).toString().padStart(2, '0')}`;
</script>

<section class="live-timeline" aria-label="实时任务进度">
  <div class="live-summary" role="status">
    <LoaderCircle size={15} class="spin"/>
    <span class="live-stage">{stage}</span>
    <span class="live-duration">{duration}</span>
  </div>

  {#each active as tool (tool.id)}
    <details class="tool-result live-running-tool" open={Boolean(tool.output)}>
      <summary><Terminal size={15}/><span class="tool-name">{toolLabel(tool.name ?? "", $toolLabels)}</span>
        <span class="tool-state pending">执行中</span><ChevronRight size={14} class="disclosure"/>
      </summary>
      {#if tool.args}<pre class="tool-arguments">{tool.args}</pre>{/if}
      {#if tool.output}<pre class="live-output">{tool.output.slice(-5000)}</pre>{/if}
    </details>
  {/each}

  {#if latestText}
    <div class="stream-markdown"><StreamingMarkdown content={latestText}/></div>
  {/if}

  {#if activities.length}
    <details class="execution-steps live-log"
      ontoggle={event => logExpanded = event.currentTarget.open}>
      <summary>
        <ChevronRight size={15} class="disclosure"/>
        <span>完整执行记录</span>
        {#if finished.length}<span class="step-summary">{finished.length} 次工具完成</span>{/if}
        {#if failed}<span class="step-fail"><CircleAlert size={13}/>{failed} 次失败</span>{/if}
      </summary>
      {#if logExpanded}
      <div class="execution-content">
        {#each activities as activity (activity.id)}
          {#if activity.kind === 'reasoning'}
            <details class="reasoning-block">
              <summary><BrainCircuit size={15}/> 思考过程 <ChevronRight size={14} class="disclosure"/></summary>
              <div class="reasoning-text">{activity.text}</div>
            </details>
          {:else if activity.kind === 'text'}
            <div class="intermediate-text"><Markdown content={activity.text}/></div>
          {:else}
            <details class="tool-result" class:failed={activity.failed}>
              <summary><Terminal size={15}/><span class="tool-name">{toolLabel(activity.name ?? "", $toolLabels)}</span>
                <span class="tool-state" class:pending={!activity.done} class:error={activity.failed}>
                  {activity.failed ? '失败' : activity.done ? '完成' : '执行中'}
                </span>
                <ChevronRight size={14} class="disclosure"/>
              </summary>
              {#if activity.args}<pre class="tool-arguments">{activity.args}</pre>{/if}
              {#if activity.output}<pre>{activity.output.slice(-15000)}</pre>{/if}
            </details>
          {/if}
        {/each}
      </div>
      {/if}
    </details>
  {/if}
</section>
