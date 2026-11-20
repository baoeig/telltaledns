<script lang="ts">
  // REQ: FLT-005 (T6.12, ADR-067) — make a quick rule: allow or block a domain for a device,
  // a group, or everyone, for a while or for good. Used in the query log's "Why?" drawer
  // (prefilled from the query) and on the Quick rules page.
  // REQ: FLT-005 (T20.1, ADR-162) — with `rule`, it edits that rule in place: the form starts
  // from the rule, saves under its ID (the rule keeps who made it and when), and the end time
  // stays as it was unless a new duration is picked. Pass a snapshot, not the live row: a list
  // that reloads must not reset what the person is typing.
  // REQ: FLT-005, FLT-017 (GitHub issue #73) — a rule can name a configured device, an IP, or a
  // CIDR, and nothing else: the names a router or a device gives an address (what the query log
  // shows for a device nobody named) are refused. So the "Why?" drawer fills in the device the
  // address is recognized as, else the address, and says what the discovered name is; a pod's
  // address changes whenever it restarts, so a Kubernetes workload gets its group. A dry run as
  // you type shows a refusal before Save.
  import { untrack } from 'svelte';
  import { api, ApiError, type S } from '../api';
  import { endsAt } from '../format';
  import ErrorNote from './ErrorNote.svelte';
  import HelpButton from './HelpButton.svelte';
  import SimulationCard from './SimulationCard.svelte';

  let {
    domain = '',
    device = '',
    group = '',
    address = '',
    seenAs = '',
    workload = '',
    rule,
    onsaved,
    oncancel,
  }: {
    domain?: string;
    /** What the Device field starts with: the configured device the address is recognized as, else the address. */
    device?: string;
    group?: string;
    /** The address the query came from (the Why? drawer): the form says what it filled in. */
    address?: string;
    /** The name the query log shows for the address when no configured device has it (a router's or mDNS). */
    seenAs?: string;
    /** The Kubernetes workload the address is a pod of: the rule starts for its group. */
    workload?: string;
    rule?: S['RuleInfo'];
    onsaved?: (message: string) => void;
    oncancel?: () => void;
  } = $props();

  // One datalist per form: the page can hold the new-rule form and an edit form together.
  const listId = `quick-devices-${Math.random().toString(36).slice(2, 8)}`;

  let action = $state<'allow' | 'block'>('allow');
  let name = $state('');
  let scope = $state<'device' | 'group' | 'everyone'>('device');
  let who = $state('');
  let whichGroup = $state('');
  let duration = $state('60');
  let note = $state('');
  let saving = $state(false);
  let error = $state<unknown>(null);
  let saved = $state('');
  let groups = $state<S['GroupInfo'][]>([]);
  let devices = $state<S['ClientInfo'][]>([]);

  $effect(() => {
    if (rule) {
      action = rule.action === 'block' ? 'block' : 'allow';
      name = rule.domain;
      who = rule.devices[0] ?? '';
      whichGroup = rule.groups[0] ?? '';
      scope = rule.devices.length ? 'device' : rule.groups.length ? 'group' : 'everyone';
      note = rule.note ?? '';
      duration = rule.expires && (rule.expiresInSeconds ?? 0) > 0 ? 'keep' : 'always';
      return;
    }
    name = domain;
    who = device;
    whichGroup = group;
    scope = workload && group ? 'group' : device ? 'device' : group ? 'group' : 'everyone';
  });
  $effect(() => {
    api.groups().then((g) => (groups = g.items)).catch(() => {});
    api.clients().then((c) => (devices = c.items)).catch(() => {});
  });

  const durations = [
    { v: '30', l: '30 minutes' },
    { v: '60', l: '1 hour' },
    { v: '120', l: '2 hours' },
    { v: 'today', l: 'Until midnight' },
    { v: '1440', l: '1 day' },
    { v: 'always', l: 'Until I remove it' },
  ];
  /** An edit can keep the end time the rule has (a new duration counts from now). */
  const options = $derived(
    rule?.expires && (rule.expiresInSeconds ?? 0) > 0
      ? [{ v: 'keep', l: `Keep the current end (${endsAt(rule.expires)})` }, ...durations]
      : durations,
  );

  /** Minutes until local midnight (at least 1). */
  function untilMidnight(): number {
    const now = new Date();
    const end = new Date(now);
    end.setHours(24, 0, 0, 0);
    return Math.max(1, Math.round((end.getTime() - now.getTime()) / 60000));
  }

  function ruleBody(): S['RuleInput'] {
    const open = duration === 'always' || duration === 'keep';
    const forMinutes = open ? undefined : duration === 'today' ? untilMidnight() : Number(duration);
    return {
      action,
      domain: name.trim(),
      devices: scope === 'device' ? [who.trim()] : [],
      groups: scope === 'group' ? [whichGroup] : [],
      expires: duration === 'keep' ? rule?.expires : undefined,
      forMinutes,
      note: note.trim() || undefined,
    };
  }

  /** REQ: OBS-024 — the rule as a dry run, replayed over the query log (nothing is saved). */
  async function simulateRule(): Promise<S['Simulation'] | null | undefined> {
    // An edit is simulated under its own ID, so it replaces the rule instead of joining it.
    const c = await api.putRule(rule?.id ?? `r-sim-${Date.now().toString(36)}`, ruleBody(), true, 'true');
    return c.simulation;
  }

  // The check as you type: the rule as a dry run, once what it names has settled, so a refusal (a
  // device a rule can't name) shows before Save, with what to do. Only a refusal of the rule
  // itself shows (422, 400); anything else waits for Save, which shows it too. Nothing is saved:
  // a new rule is checked under an ID of its own that is never stored.
  const checkId = `r-check-${Math.random().toString(36).slice(2, 8)}`;
  let checkError = $state<unknown>(null);
  let checkSeq = 0;
  $effect(() => {
    const ready = name.trim() !== '' && (scope === 'device' ? who.trim() !== '' : scope === 'group' ? whichGroup !== '' : true);
    const mine = ++checkSeq;
    checkError = null;
    if (!ready) return;
    const body = untrack(ruleBody);
    const t = setTimeout(() => {
      api
        .putRule(rule?.id ?? checkId, body, true)
        .then(() => {
          if (mine === checkSeq) checkError = null;
        })
        .catch((err) => {
          if (mine === checkSeq && err instanceof ApiError && (err.status === 422 || err.status === 400)) checkError = err;
        });
    }, 400);
    return () => clearTimeout(t);
  });

  async function save(e: SubmitEvent) {
    e.preventDefault();
    saving = true;
    error = null;
    saved = '';
    const body = ruleBody();
    const id = rule?.id ?? `r-${Date.now().toString(36)}`;
    try {
      const c = await api.putRule(id, body);
      saved = c.impact ?? 'Saved.';
      onsaved?.(saved);
    } catch (err) {
      error = err;
    } finally {
      saving = false;
    }
  }
</script>

<form class="quick" onsubmit={save} data-testid={rule ? 'quick-rule-edit-form' : 'quick-rule-form'}>
  <div class="row">
    <span class="seg" role="group" aria-label="Action">
      <button type="button" aria-pressed={action === 'allow'} onclick={() => (action = 'allow')}>Allow</button>
      <button type="button" aria-pressed={action === 'block'} onclick={() => (action = 'block')}>Block</button>
    </span>
    <input class="grow mono" aria-label="Domain" placeholder="game.example.com" bind:value={name} required />
  </div>
  <div class="row">
    <label class="small">For
      <select bind:value={scope} aria-label="Applies to">
        <option value="device">a device</option>
        <option value="group">a group</option>
        <option value="everyone">everyone</option>
      </select>
    </label>
    {#if scope === 'device'}
      <input aria-label="Device" list={listId} placeholder="named device, IP, or network" bind:value={who} required />
      <datalist id={listId}>{#each devices as d (d.name)}<option value={d.name}></option>{/each}</datalist>
    {:else if scope === 'group'}
      <select aria-label="Group" bind:value={whichGroup} required>
        {#each groups as g (g.name)}<option value={g.name}>{g.name}</option>{/each}
      </select>
    {/if}
    <label class="small">for
      <select aria-label="Duration" bind:value={duration}>
        {#each options as d (d.v)}<option value={d.v}>{d.l}</option>{/each}
      </select>
    </label>
  </div>
  <div class="row">
    <input class="grow" aria-label="Note" placeholder="Note (optional), e.g. Mom's game" bind:value={note} />
    <button class="primary" disabled={saving || !name.trim()}>{saving ? 'Saving…' : rule ? 'Save changes' : 'Save rule'}</button>
    {#if rule}<button type="button" class="link" onclick={oncancel}>Cancel</button>{/if}
  </div>
  {#if !rule && address}
    <!-- REQ: FLT-005, FLT-017 (issue #73) — what the form filled in, and what the name in the log is. -->
    {#if scope === 'group' && workload}
      <p class="muted small" data-testid="quick-rule-seen-as">
        {workload} is a Kubernetes workload and its pods come and go, so this rule is for its group, {whichGroup}: every device in it. For one
        address instead, choose “a device”.
      </p>
    {:else if scope === 'device' && device !== address && who.trim() === device}
      <p class="muted small" data-testid="quick-rule-seen-as">{address} is recognized as the device “{device}”.</p>
    {:else if scope === 'device' && workload && who.trim() === address}
      <p class="muted small" data-testid="quick-rule-seen-as">
        {address} is one pod of {workload}, and a pod's address changes whenever it restarts. For every pod, choose “a group”.
      </p>
    {:else if scope === 'device' && who.trim() === address}
      <p class="muted small" data-testid="quick-rule-seen-as">
        {address}{seenAs && seenAs !== address ? ` (${seenAs}, as your router or the device names it)` : ''}. A rule for an address stops matching
        when the address changes. Name this device to make rules follow it.<HelpButton id="device-name" />
      </p>
    {/if}
  {/if}
  <p class="muted small">
    Includes subdomains. Decides before any list. Devices may keep an old answer for a few minutes.{#if rule} Saving keeps who made the
    rule and when.{/if}
  </p>
  <div data-testid="quick-rule-check">{#if !error}<ErrorNote error={checkError} />{/if}</div>
  <ErrorNote {error} />
  {#if saved && !rule}<div class="notice ok small" data-testid="quick-rule-saved">{saved}</div>{/if}
  {#if name.trim() && (scope !== 'device' || who.trim())}<SimulationCard run={simulateRule} />{/if}
</form>

<style>
  .quick {
    display: grid;
    gap: 10px;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    align-items: center;
  }
  .grow {
    flex: 1 1 200px;
  }
  label {
    display: inline-flex;
    gap: 6px;
    align-items: center;
  }
</style>
