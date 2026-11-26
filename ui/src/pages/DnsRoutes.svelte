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
  import { tr } from '../lib/i18n';
  import T from '../lib/i18n/T.svelte';

  const { t } = tr('dnsRoutes');
  const words = {
    stale: t('Using last good'),
    byUi: t('the UI / API'),
    byFile: t('config file'),
    edit: t('Edit'),
    remove: t('Remove'),
  };

  type Forward = S['ForwardInfo'];
  type Editor = { name: string; url: string; servers: string; upstreamGroup?: string; useAddresses: boolean; editing: boolean };
  let subscriptions = $state<Forward[] | null>(null);
  let error = $state<unknown>(null);
  let editor = $state<Editor | null>(null);
  let formError = $state<unknown>(null);
  let busy = $state(false);
  let saved = $state(false);
  let latestLoad = 0;
  const canEdit = $derived(session.user?.role === 'admin' || session.user?.role === 'operator');
  const addresses = $derived(editor?.servers.split(/[\s,]+/).filter(Boolean) ?? []);
  const destinationReady = $derived(editor && (!editor.useAddresses || addresses.length > 0));

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
      upstreamGroup: forward?.upstreamGroup ?? undefined,
      useAddresses: !forward?.upstreamGroup,
      editing: !!forward,
    };
    formError = null;
    saved = false;
  }

  async function save(event: SubmitEvent) {
    event.preventDefault();
    const draft = editor;
    if (!draft || !destinationReady) return;
    const { name, url } = draft;
    // REQ: UPS-007 — existing group bindings stay in effect until explicitly replaced.
    const upstreamGroup = draft.useAddresses ? undefined : draft.upstreamGroup;
    busy = true;
    formError = null;
    saved = false;
    try {
      await api.putForward(name.trim(), {
        subscriptionUrl: url.trim(),
        ...(upstreamGroup ? { upstreamGroup } : { servers: addresses }),
      });
      if (editor === draft) {
        if (!upstreamGroup) {
          draft.upstreamGroup = undefined;
          draft.useAddresses = true;
        }
        saved = true;
      }
      await load();
    } catch (e) {
      if (editor === draft) formError = e;
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

{#snippet data(text: string)}<span translate="no">{text}</span>{/snippet}
{#snippet local()}<a href={href('/local-dns')}>{t('Names on my network')}</a>{/snippet}

<div class="page">
  <div class="page-head">
    <h1>{t('DNS Routes')}<HelpButton id="route-subscriptions" /></h1>
    {#if canEdit}
      <button class="primary" onclick={() => openEditor()}>{t('Add a route subscription')}</button>
    {/if}
  </div>
  <ErrorNote {error} />
  <section class="card">
    <h2>{t('Route subscriptions')}</h2>
    {#if subscriptions === null}
      {#if !error}<p class="muted">{t('Loading route subscriptions…')}</p>{/if}
    {:else if subscriptions.length === 0}
      <p class="empty">{t('Add a subscription to send matching domains to a dedicated DNS server.')}</p>
    {:else}
      <div class="table-wrap">
        <table>
          <thead><tr><th>{t('List')}</th><th>{t('State')}</th><th class="num">{t('Names used')}</th><th class="num">{t('Exceptions')}</th><th>{t('DNS servers')}</th><th>{t('Defined in')}</th><th></th></tr></thead>
          <tbody>
            {#each subscriptions as forward (forward.domain)}
              {@const status = forward.subscriptionStatus}
              {@const state = listState(status)}
              <tr>
                <td>
                  <strong translate="no">{forward.domain}</strong>
                  <div class="muted small source" translate="no">{forward.subscriptionUrl}</div>
                  {#if status?.error}<div class="small err" translate="no">{status.error}</div>{/if}
                </td>
                <td><StatusBadge value={state} label={state === 'stale' ? words.stale : undefined} /></td>
                <td class="num">{status?.namesUsed == null ? '–' : num(status.namesUsed)}</td>
                <td class="num">{status?.exceptions == null ? '–' : num(status.exceptions)}</td>
                <td>
                  <div class="mono small source" translate="no">{forward.servers.join(', ')}</div>
                  {#if forward.upstreamGroup}<div class="muted small"><T ctx="dnsRoutes" msg={'Group <data>{group}</data>'} params={{ group: forward.upstreamGroup }} {data} /></div>{/if}
                </td>
                <td class="small">{forward.source === 'api' ? words.byUi : words.byFile}</td>
                <td class="num nowrap">
                  {#if canEdit && forward.source === 'api'}
                    <button class="link small" onclick={() => openEditor(forward)}>{words.edit}</button>
                    <button class="link small" onclick={() => remove(forward.domain)} aria-label={t('Remove {name}', { name: forward.domain })}>{words.remove}</button>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      <p class="muted small">{t('“Names used” counts the domain suffixes in the active subscription. “Exceptions” counts suffixes exempt from this route.')}</p>
    {/if}
  </section>
  <p class="muted small"><T ctx="dnsRoutes" msg={'For single-domain forwarding, see <local/>.'} {local} /></p>
</div>

{#if editor}
  <Drawer title={editor.editing ? t('Edit route subscription') : t('Add a route subscription')} onclose={() => (editor = null)}>
    <form class="stack" onsubmit={save}>
      <label>{t('Subscription name')} <input bind:value={editor.name} placeholder={t('gfwlist')} pattern="[a-z0-9_\-]+" maxlength="64" required readonly={editor.editing} /></label>
      <label>{t('GFWList URL')} <input type="url" bind:value={editor.url} placeholder={t('https://raw.githubusercontent.com/gfwlist/gfwlist/master/gfwlist.txt')} required /></label>
      {#if editor.upstreamGroup}
        <label class="row">
          <input type="checkbox" bind:checked={editor.useAddresses} /> <T ctx="dnsRoutes" msg={'Replace group <data>{group}</data> with manual addresses'} params={{ group: editor.upstreamGroup }} {data} />
        </label>
      {/if}
      <label>{t('Server address')} <input bind:value={editor.servers} placeholder={t('https://cloudflare-dns.com/dns-query')} disabled={!editor.useAddresses} required={editor.useAddresses} /></label>
      {#if editor.useAddresses}
        <p class="muted small">{t('Separate multiple servers with spaces or commas. They are tried in order.')}</p>
      {/if}
      <ErrorNote error={formError} />
      {#if saved}<p class="notice ok" role="status">{t('Subscription saved. The list will refresh in the background.')}</p>{/if}
      <div class="row"><button type="submit" class="primary" disabled={busy || !destinationReady}>{busy ? t('Saving…') : t('Save')}</button></div>
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
