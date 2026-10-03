// The lucide icon component for each describe().icon key (lib/tools.js), shared by ToolCall
// and the approval card so a tool's icon is the same wherever it appears.
import IconFileText from '~icons/lucide/file-text'
import IconFolder from '~icons/lucide/folder'
import IconSearch from '~icons/lucide/search'
import IconFileDiff from '~icons/lucide/file-diff'
import IconFilePlus from '~icons/lucide/file-plus'
import IconTerminal from '~icons/lucide/terminal'
import IconListChecks from '~icons/lucide/list-checks'
import IconBrain from '~icons/lucide/brain'
import IconGlobe from '~icons/lucide/globe'
import IconLink from '~icons/lucide/link'
import IconMessagesSquare from '~icons/lucide/messages-square'
import IconPuzzle from '~icons/lucide/puzzle'
import IconCrosshair from '~icons/lucide/crosshair'
import IconWrench from '~icons/lucide/wrench'
import IconCircleHelp from '~icons/lucide/circle-help'
import IconMap from '~icons/lucide/map'
import IconUsers from '~icons/lucide/users'
import IconImage from '~icons/lucide/image'

const TOOL_ICONS = {
  'file-text': IconFileText,
  folder: IconFolder,
  search: IconSearch,
  'file-diff': IconFileDiff,
  'file-plus': IconFilePlus,
  terminal: IconTerminal,
  'list-checks': IconListChecks,
  brain: IconBrain,
  globe: IconGlobe,
  link: IconLink,
  'messages-square': IconMessagesSquare,
  puzzle: IconPuzzle,
  crosshair: IconCrosshair,
  wrench: IconWrench,
  'circle-help': IconCircleHelp,
  map: IconMap,
  users: IconUsers,
  image: IconImage,
}

export const toolIcon = (key) => TOOL_ICONS[key] ?? IconWrench
