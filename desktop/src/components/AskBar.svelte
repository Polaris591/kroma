<script lang="ts">
  import { onMount } from 'svelte';
  import { CircleHelp, Send, X } from '@lucide/svelte';
  import {
    buildAnswer, canAnswer, dismissAnswer, moveCursor, togglePick,
    type AskAnswer, type AskQuestion,
  } from '../lib/ask-question';

  export let ask: AskQuestion;
  export let onAnswer: (answer: AskAnswer) => void;

  // -1 means no option is highlighted, so a typed answer alone is a valid reply.
  let cursor = -1;
  let picked: string[] = [];
  let typed = '';
  let field: HTMLInputElement;

  onMount(() => { field?.focus(); });

  $: ready = canAnswer(picked, typed);

  function pick(option: string, index: number, viaMouse: boolean) {
    cursor = index;
    picked = togglePick(picked, option, ask.multiple);
    // A mouse click hands the keyboard back to the text field; Tab navigation
    // keeps its place so several options can be picked without a mouse.
    if (viaMouse) field?.focus();
  }

  function confirm() {
    if (!ready) return;
    onAnswer(buildAnswer(ask, picked, typed));
  }

  function dismiss() {
    onAnswer(dismissAnswer(ask.id));
  }

  // Keys are handled on the focused text field, which is where they arrive.
  function onKeydown(event: KeyboardEvent) {
    if (event.isComposing) return;
    if (event.key === 'Escape') {
      event.preventDefault();
      dismiss();
    } else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      if (!ask.options.length) return;
      event.preventDefault();
      cursor = moveCursor(cursor, event.key === 'ArrowDown' ? 1 : -1, ask.options.length);
      if (!ask.multiple) picked = [ask.options[cursor]];
    } else if (event.key === 'Enter') {
      event.preventDefault();
      confirm();
    }
  }
</script>

<div class="composer ask-bar" role="group" aria-label="插件提问">
  <div class="ask-head">
    <CircleHelp size={16}/>
    <p>{ask.question}</p>
    <button class="icon-button" title="关闭（视为未回答）" aria-label="关闭问题" onclick={dismiss}><X size={15}/></button>
  </div>
  {#if ask.options.length}
    <div class="ask-options" role="listbox" aria-multiselectable={ask.multiple} aria-label="可选项">
      {#each ask.options as option, index}
        <button class="ask-option" class:active={index === cursor} class:chosen={picked.includes(option)}
          role="option" aria-selected={picked.includes(option)}
          onmouseenter={() => cursor = index}
          onclick={(event) => pick(option, index, event.detail > 0)}>{option}</button>
      {/each}
    </div>
  {/if}
  <div class="ask-answer">
    <input bind:this={field} bind:value={typed} onkeydown={onKeydown}
      placeholder="或者直接输入你的回答…" aria-label="输入回答"/>
    <button class="send-button" title="回答（Enter）" aria-label="回答" disabled={!ready} onclick={confirm}><Send size={16}/></button>
  </div>
  <div class="ask-tip">↑↓ 选择 · Enter 回答 · Esc 关闭（视为未回答）</div>
</div>
