/** How an avatar should look for what its bot is doing. */
export const moodOf = (bot) => (bot.status === 'needs_input' ? 'needsInput' : bot.status === 'working' ? 'working' : 'idle')
