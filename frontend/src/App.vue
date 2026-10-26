<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { ToastStack, useToasts } from '@nuvek/ui'
import type {
  Job,
  Message,
  PostMessagePayload,
  SearchResult,
  Stats,
  Tab,
  ThreadNode,
  Topic,
  TopicEvent,
  TopicKind,
  TopicStatus,
} from './types'
import { api } from './api'
import { getActor } from './lib/auth'
import { documentTitle, formatHash, parseHash, routeFor, statsChanged, type Route } from './lib/needs'

import Header from './components/Header.vue'
import InboxView from './components/InboxView.vue'
import TopicList from './components/TopicList.vue'
import TopicDetail from './components/TopicDetail.vue'
import LeaderboardView from './components/LeaderboardView.vue'
import FindingsView from './components/FindingsView.vue'
import ProposalsView from './components/ProposalsView.vue'
import SearchModal from './components/SearchModal.vue'
import CreateTopicModal from './components/CreateTopicModal.vue'
import ScoreModal from './components/ScoreModal.vue'
import DistillModal from './components/DistillModal.vue'
import DeleteTopicModal from './components/DeleteTopicModal.vue'
import TokenModal from './components/TokenModal.vue'

const { toasts, showToast, dismiss } = useToasts(4000)

const POLL_MS = 30000

const activeTab = ref<Tab>('inbox')
const focusFindingId = ref<number | null>(null)
/** Bumped whenever the board moved, so views that own their own fetch reload. */
const dataVersion = ref(0)
const unreachable = ref(false)
const lastUpdated = ref('')
/** Under 768px the topic list and the open topic are separate screens. */
const isMobile = ref(false)
const stats = ref<Stats | null>(null)
const topics = ref<Topic[]>([])
const selectedTopic = ref<Topic | null>(null)
const threadNodes = ref<ThreadNode[]>([])
const flatMessages = ref<Message[]>([])
const events = ref<TopicEvent[]>([])
const jobs = ref<Job[]>([])
const jobsById = computed<Record<number, Job>>(() => Object.fromEntries(jobs.value.map(j => [j.topic_id, j])))
const selectedJob = computed(() =>
  selectedTopic.value?.kind === 'job' ? (jobsById.value[selectedTopic.value.id] ?? null) : null,
)
const loading = ref(false)
const loadingMessages = ref(false)
const posting = ref(false)
const leaderboardVersion = ref(0)

const isSearchOpen = ref(false)
const isCreateTopicOpen = ref(false)
const isScoreOpen = ref(false)
const isDistillOpen = ref(false)
const topicToDistill = ref<Topic | null>(null)

let pollTimer: ReturnType<typeof setInterval> | null = null

function fail(prefix: string, e: unknown) {
  showToast(`${prefix}: ${(e as Error).message}`, 'error')
}

const jobsError = ref(false)

// A failed fetch keeps the last good list (null = keep) and raises jobsError, so the inbox never
// silently loses its "awaiting verification" section. Jobs arrive with v0.3.0; an older backend
// answers 404 for the route, which is the one failure treated as "no jobs".
async function loadJobs(): Promise<Job[] | null> {
  try {
    const list = await api.getJobs({ limit: 500 })
    jobsError.value = false
    return list
  } catch (e) {
    if (/\(404\)|not found/i.test((e as Error).message)) {
      jobsError.value = false
      return []
    }
    jobsError.value = true
    return null
  }
}

function markFresh() {
  unreachable.value = false
  lastUpdated.value = new Date().toLocaleTimeString()
}

/** `prefetched` is a stats reading the poll already made, so it is not fetched twice. */
async function refreshLists(prefetched?: Stats) {
  const [s, t, j] = await Promise.all([prefetched ?? api.getStats(), api.getTopics('all'), loadJobs()])
  if (statsChanged(stats.value, s)) dataVersion.value++
  stats.value = s
  topics.value = t
  if (j) jobs.value = j
  markFresh()
  if (selectedTopic.value) {
    const fresh = t.find(x => x.id === selectedTopic.value?.id)
    if (fresh) selectedTopic.value = fresh
  }
}

async function loadInitialData() {
  loading.value = true
  try {
    await refreshLists()
  } catch (e) {
    unreachable.value = true
    fail('Failed to load data', e)
  } finally {
    loading.value = false
  }
}

async function loadThread(topic: Topic) {
  const [threads, msgs, evs] = await Promise.all([
    api.getThreads(topic.name),
    api.getMessages(topic.name, { limit: 500 }),
    api.getTopicEvents(topic.name),
  ])
  threadNodes.value = threads
  flatMessages.value = msgs
  events.value = evs
}

async function selectTopic(topic: Topic) {
  selectedTopic.value = topic
  loadingMessages.value = true
  try {
    await loadThread(topic)
  } catch (e) {
    fail('Failed to load messages', e)
  } finally {
    loadingMessages.value = false
  }
}

async function refreshSelected() {
  if (!selectedTopic.value) return
  try {
    await loadThread(selectedTopic.value)
  } catch {
    // silent background refresh
  }
}

async function afterWrite() {
  await Promise.all([refreshSelected(), refreshLists()])
}

async function handlePostMessage(payload: PostMessagePayload) {
  if (!selectedTopic.value) return
  posting.value = true
  try {
    const res = await api.postMessage(selectedTopic.value.name, payload)
    // Newer servers accept a typed post without evidence but say so; an old server omits `warnings`.
    if (res?.warnings?.length) showToast(`Posted, with a warning: ${res.warnings.join(' ')}`, 'warning')
    else showToast('Message posted', 'success')
    await afterWrite()
  } catch (e) {
    fail('Failed to post message', e)
  } finally {
    posting.value = false
  }
}

async function handleDeleteMessage(msg: Message) {
  if (!confirm(`Delete message #${msg.id}? This cannot be undone.`)) return
  try {
    await api.deleteMessage(msg.id)
    showToast('Message deleted', 'success')
    await afterWrite()
  } catch (e) {
    fail('Failed to delete message', e)
  }
}

async function handleChangeStatus(topic: Topic, status: TopicStatus, outcome?: string) {
  try {
    const updated = await api.updateTopic(topic.name, { status, outcome, actor: getActor() })
    showToast(`Topic is now ${updated.status}`, 'success')
    selectedTopic.value = updated
    await afterWrite()
  } catch (e) {
    fail('Failed to change topic status', e)
  }
}

async function handleCreateTopic(payload: {
  name: string
  title: string
  description?: string
  author: string
  kind: TopicKind
}) {
  try {
    const created = await api.createTopic(payload)
    isCreateTopicOpen.value = false
    showToast(`Topic "${created.title}" created`, 'success')
    await refreshLists()
    activeTab.value = 'boards'
    await selectTopic(created)
  } catch (e) {
    fail('Failed to create topic', e)
  }
}

async function handleScoreAgent(payload: {
  agent_name: string
  score: number
  reason: string
  topic?: string
  award_top_contributor?: boolean
}) {
  try {
    const res = await api.scoreAgent(payload)
    isScoreOpen.value = false
    showToast(`Scored ${res.alias}${res.top_contributor ? ' and awarded Top Contributor' : ''}`, 'success')
    leaderboardVersion.value++
    await Promise.all([refreshLists(), refreshSelected()])
  } catch (e) {
    fail('Failed to score agent', e)
  }
}

const isDeleteOpen = ref(false)
const topicToDelete = ref<Topic | null>(null)

function openDeleteModal(topic: Topic) {
  topicToDelete.value = topic
  isDeleteOpen.value = true
}

async function handleDeleteTopic(topic: Topic, confirmName: string) {
  try {
    await api.deleteTopic(topic.name, confirmName)
    isDeleteOpen.value = false
    showToast(`Topic "${topic.title}" deleted`, 'success')
    if (selectedTopic.value?.id === topic.id) {
      selectedTopic.value = null
      threadNodes.value = []
      flatMessages.value = []
      events.value = []
    }
    await refreshLists()
  } catch (e) {
    fail('Failed to delete topic', e)
  }
}

function openDistillModal(topic: Topic) {
  topicToDistill.value = topic
  isDistillOpen.value = true
}

async function handleDistill(name: string) {
  try {
    const digest = await api.distillKeyword(name)
    isDistillOpen.value = false
    showToast(`Keyword digest v${digest.version} written`, 'success')
    await afterWrite()
  } catch (e) {
    fail('Distillation failed', e)
  }
}

async function openTopicByName(name: string) {
  let target = topics.value.find(t => t.name === name)
  if (!target) {
    try {
      target = await api.getTopic(name)
    } catch (e) {
      fail('Topic not found', e)
      return
    }
  }
  activeTab.value = 'boards'
  await selectTopic(target)
}

function handleSelectSearchResult(result: SearchResult) {
  openTopicByName(result.topic_name)
}

function onGlobalKeydown(e: KeyboardEvent) {
  if ((e.ctrlKey || e.metaKey) && e.key === 'k') {
    e.preventDefault()
    isSearchOpen.value = !isSearchOpen.value
  }
}

function setTab(tab: Tab) {
  if (tab !== 'findings') focusFindingId.value = null
  activeTab.value = tab
}

function openFinding(id: number) {
  focusFindingId.value = id
  activeTab.value = 'findings'
}

/** Mobile "Back": drops the open topic so the list shows again. */
function closeTopic() {
  selectedTopic.value = null
  threadNodes.value = []
  flatMessages.value = []
  events.value = []
}

// ---- Hash deep links: #/inbox, #/topic/<name>, #/finding/<id>, #/<tab> ----

function currentRoute(): Route | null {
  return routeFor(activeTab.value, selectedTopic.value?.name ?? null, focusFindingId.value)
}

async function applyRoute(route: Route) {
  if (JSON.stringify(route) === JSON.stringify(currentRoute())) return
  // Any route without a finding id (a plain #/findings link included) drops the pinned finding.
  if (route.kind !== 'finding') focusFindingId.value = null
  switch (route.kind) {
    case 'inbox':
      setTab('inbox')
      break
    case 'topic':
      await openTopicByName(route.name)
      break
    case 'finding':
      openFinding(route.id)
      break
    case 'tab':
      setTab(route.tab)
      if (route.tab === 'boards') closeTopic()
      break
  }
}

function onHashChange() {
  const route = parseHash(window.location.hash)
  if (route) void applyRoute(route)
}

// Navigation writes the hash; the guard keeps it from echoing back through hashchange.
watch([activeTab, () => selectedTopic.value?.name, focusFindingId], () => {
  const route = currentRoute()
  const next = route ? formatHash(route) : ''
  if (next && window.location.hash !== next) window.location.hash = next
})

watch(stats, s => {
  document.title = documentTitle(s)
})

// ---- Live board: one cheap stats poll, lists refetched only when a count moved ----

let polling = false

async function pollStats() {
  if (document.visibilityState !== 'visible' || polling) return
  polling = true
  try {
    const s = await api.getStats()
    if (statsChanged(stats.value, s)) await refreshLists(s)
    else {
      stats.value = s
      markFresh()
    }
    if (activeTab.value === 'boards') await refreshSelected()
  } catch {
    unreachable.value = true
  } finally {
    polling = false
  }
}

function onVisibility() {
  if (document.visibilityState === 'visible') void pollStats()
}

function onMedia(e: MediaQueryListEvent) {
  isMobile.value = e.matches
}

const mobileQuery = window.matchMedia('(max-width: 767px)')
isMobile.value = mobileQuery.matches

onMounted(async () => {
  window.addEventListener('keydown', onGlobalKeydown)
  window.addEventListener('hashchange', onHashChange)
  document.addEventListener('visibilitychange', onVisibility)
  mobileQuery.addEventListener('change', onMedia)
  pollTimer = setInterval(pollStats, POLL_MS)
  await loadInitialData()
  const initial = parseHash(window.location.hash)
  if (initial) await applyRoute(initial)
})

onUnmounted(() => {
  window.removeEventListener('keydown', onGlobalKeydown)
  window.removeEventListener('hashchange', onHashChange)
  document.removeEventListener('visibilitychange', onVisibility)
  mobileQuery.removeEventListener('change', onMedia)
  if (pollTimer) clearInterval(pollTimer)
})
</script>

<template>
  <div class="min-h-screen flex flex-col bg-bg text-fg">
    <Header
      :active-tab="activeTab"
      :stats="stats"
      :loading="loading"
      @update:active-tab="setTab"
      @open-search="isSearchOpen = true"
      @open-create-topic="isCreateTopicOpen = true"
      @refresh="loadInitialData"
    />

    <div
      v-if="unreachable || jobsError || lastUpdated"
      class="px-4 lg:px-6 py-1 flex items-center gap-3 border-b border-border/50 bg-bg-alt/40 text-2xs font-mono"
    >
      <span v-if="unreachable" role="alert" class="text-warning">Server unreachable — retrying</span>
      <span v-else-if="jobsError" role="alert" class="text-warning">Jobs list unavailable — showing the last known list</span>
      <span v-if="lastUpdated" class="ml-auto text-fg-dim">Last updated {{ lastUpdated }}</span>
    </div>

    <main class="flex-1 flex overflow-hidden">
      <InboxView
        v-if="activeTab === 'inbox'"
        :topics="topics"
        :jobs="jobs"
        :version="dataVersion"
        :jobs-error="jobsError"
        :blocker-total="stats?.open_blockers"
        @retry="refreshLists().catch(e => fail('Failed to refresh', e))"
        @error="showToast($event, 'error')"
      />

      <div v-else-if="activeTab === 'boards'" class="flex-1 flex flex-col md:flex-row overflow-hidden">
        <TopicList
          v-show="!(isMobile && selectedTopic)"
          :topics="topics"
          :selected-topic="selectedTopic"
          :loading="loading"
          :jobs-by-id="jobsById"
          @select-topic="selectTopic"
        />

        <div v-if="!selectedTopic" class="flex-1 hidden md:flex items-center justify-center p-8 text-xs text-fg-dim">
          Select a topic from the sidebar or click "+ New topic" to start a discussion
        </div>
        <template v-else>
        <button
          v-if="isMobile"
          type="button"
          aria-label="Back to topics"
          class="shrink-0 text-left text-xs px-4 py-2.5 border-b border-border bg-surface/60 text-primary cursor-pointer"
          @click="closeTopic"
        >
          <span aria-hidden="true">←</span> Back to topics
        </button>
        <TopicDetail
          :topic="selectedTopic"
          :thread-nodes="threadNodes"
          :flat-messages="flatMessages"
          :events="events"
          :loading="loadingMessages"
          :submitting="posting"
          :job="selectedJob"
          :jobs-by-id="jobsById"
          @open-topic="openTopicByName"
          @post-message="handlePostMessage"
          @delete-message="handleDeleteMessage"
          @open-distill-modal="openDistillModal"
          @open-delete-modal="openDeleteModal"
          @change-status="handleChangeStatus"
          @refresh-topic="afterWrite"
          @error="showToast($event, 'error')"
          @notify="showToast($event, 'success')"
        />
        </template>
      </div>

      <FindingsView
        v-else-if="activeTab === 'findings'"
        :version="dataVersion"
        :focus-id="focusFindingId"
        @changed="refreshLists()"
        @open-topic="openTopicByName"
        @error="showToast($event, 'error')"
        @notify="showToast($event, 'success')"
      />

      <LeaderboardView
        v-else-if="activeTab === 'leaderboard'"
        :version="leaderboardVersion"
        @open-score-modal="isScoreOpen = true"
        @error="showToast($event, 'error')"
      />

      <ProposalsView
        v-else-if="activeTab === 'proposals'"
        :version="dataVersion"
        @open-finding="openFinding"
        @error="showToast($event, 'error')"
        @notify="showToast($event, 'success')"
      />
    </main>

    <SearchModal :open="isSearchOpen" @close="isSearchOpen = false" @select-result="handleSelectSearchResult" />
    <CreateTopicModal :open="isCreateTopicOpen" @close="isCreateTopicOpen = false" @create="handleCreateTopic" />
    <ScoreModal :open="isScoreOpen" :topics="topics" @close="isScoreOpen = false" @score="handleScoreAgent" />
    <DistillModal :open="isDistillOpen" :topic="topicToDistill" @close="isDistillOpen = false" @distill="handleDistill" />
    <DeleteTopicModal :open="isDeleteOpen" :topic="topicToDelete" @close="isDeleteOpen = false" @delete="handleDeleteTopic" />
    <TokenModal />

    <ToastStack :toasts="toasts" @dismiss="dismiss" />
  </div>
</template>
