<script lang="ts">
  // REQ: DNS-017, UPS-007 — manage route subscriptions through the forwarding API.
  import { api, type S } from '../lib/api';
  import { num } from '../lib/format';
  import { poll } from '../lib/poll';
  import { href } from '../lib/router.svelte';
  import { session } from '../lib/session.svelte';
  import Drawer from '../lib/components/Drawer.svelte';
  import ErrorNote from '../lib/components/ErrorNote.svelte';
  import HelpButton from '../lib/components/HelpButton.svelte';
  import StatusBadge from '../lib/components/StatusBadge.svelte';

  type Forward = S['ForwardInfo'];
  let subscriptions = $state<Forward[] | null>(null);
  let error = $state<unknown>(null);
  let editor = $state<{ name: string; url: string; servers: string; editing: boolean } | null>(null);
  let formError = $state<unknown>(null);
  let busy = $state(false);
  let saved = $state(false);
  let latestLoad = 0;
  const canEdit = $derived(session.user?.role === 'admin' || session.user?.role === 'operator');

  async function load() {
    const request = ++latestLoad;
    try {
      const { items } = await api.forwards();
      if (request !== latestLoad) return;
      subscriptions = items.filter((forward) => forward.subscriptionUrl);
      error = null;
    } catch (e) {
      if (request === latestLoad) error = e;
    }
  }
  $effect(() => poll(load, 15000));

  function listState(status: Forward['subscriptionStatus']) {
    if (status?.error) return status.namesUsed == null ? 'failed' : 'stale';
    return status?.namesUsed == null ? 'pending' : 'ok';
  }

  function openEditor(forward?: Forward) {
    editor = {
      name: forward?.domain ?? '',
      url: forward?.subscriptionUrl ?? '',
      servers: forward?.servers.join(' ') ?? '',
      editing: !!forward,
    };
    formError = null;
    saved = false;
  }

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (!editor) return;
    busy = true;
    formError = null;
    saved = false;
    try {
      await api.putForward(editor.name.trim(), {
        servers: editor.servers.split(/[\s,]+/).filter(Boolean),
        subscriptionUrl: editor.url.trim(),
      });
      saved = true;
      await load();
    } catch (e) {
      formError = e;
    } finally {
      busy = false;
    }
  }

  async function remove(name: string) {
    try {
      await api.deleteForward(name);
      await load();
    } catch (e) {
      error = e;
    }
  }
</script>

<div class="page">
  <div class="page-head">
    <h1>DNS Routes<HelpButton id="route-subscriptions" /></h1>
    {#if canEdit}
      <button class="primary" onclick={() => openEditor()}>Add a route subscription</button>
    {/if}
  </div>
  <ErrorNote {error} />
  <section class="card">
    <h2>Route subscriptions</h2>
    {#if subscriptions === null}
      {#if !error}<p class="muted">Loading route subscriptions…</p>{/if}
    {:else if subscriptions.length === 0}
      <p class="empty">Add a subscription to send matching domains to a dedicated DNS server.</p>
    {:else}
      <div class="table-wrap">
        <table>
          <thead><tr><th>List</th><th>State</th><th class="num">Names used</th><th class="num">Exceptions</th><th>DNS servers</th><th>Defined in</th><th></th></tr></thead>
          <tbody>
            {#each subscriptions as forward (forward.domain)}
              {@const status = forward.subscriptionStatus}
              {@const state = listState(status)}
              <tr>
                <td>
                  <strong>{forward.domain}</strong>
                  <div class="muted small source">{forward.subscriptionUrl}</div>
                  {#if status?.error}<div class="small err">{status.error}</div>{/if}
                </td>
                <td><StatusBadge value={state} label={state === 'stale' ? 'Using last good' : undefined} /></td>
                <td class="num">{status?.namesUsed == null ? '–' : num(status.namesUsed)}</td>
                <td class="num">{status?.exceptions == null ? '–' : num(status.exceptions)}</td>
                <td class="mono small">{forward.servers.join(', ')}</td>
                <td class="small">{forward.source === 'api' ? 'the UI / API' : 'config file'}</td>
                <td class="num nowrap">
                  {#if canEdit && forward.source === 'api'}
                    <button class="link small" onclick={() => openEditor(forward)}>Edit</button>
                    <button class="link small" onclick={() => remove(forward.domain)} aria-label={`Remove ${forward.domain}`}>Remove</button>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      <p class="muted small">“Names used” counts the domain suffixes in the active subscription. “Exceptions” counts suffixes exempt from this route.</p>
    {/if}
  </section>
  <p class="muted small">For single-domain forwarding, see <a href={href('/local-dns')}>Names on my network</a>.</p>
</div>

{#if editor}
  <Drawer title={editor.editing ? 'Edit route subscription' : 'Add a route subscription'} onclose={() => (editor = null)}>
    <form class="stack" onsubmit={save}>
      <label>Subscription name <input bind:value={editor.name} placeholder="gfwlist" pattern="[a-z0-9_\-]+" maxlength="64" required readonly={editor.editing} /></label>
      <label>GFWList URL <input type="url" bind:value={editor.url} placeholder="https://raw.githubusercontent.com/gfwlist/gfwlist/master/gfwlist.txt" required /></label>
      <label>Server address <input bind:value={editor.servers} placeholder="https://cloudflare-dns.com/dns-query" required /></label>
      <p class="muted small">Separate multiple servers with spaces. They are tried in order.</p>
      <ErrorNote error={formError} />
      {#if saved}<p class="notice ok" role="status">Subscription saved. The list will refresh in the background.</p>{/if}
      <div class="row"><button type="submit" class="primary" disabled={busy}>{busy ? 'Saving…' : 'Save'}</button></div>
    </form>
  </Drawer>
{/if}

<style>
  .stack {
    display: grid;
    gap: 12px;
  }
  .source {
    overflow-wrap: anywhere;
  }
</style>
