'use strict';

/* Clawdy icon system — SF Symbols–style SVG primitives */
window.ClawdyIcons = {
  logo(size = 28) {
    return `<svg width="${size}" height="${size}" viewBox="0 0 28 28" fill="none" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
      <rect x="1" y="1" width="26" height="26" rx="7.5" fill="url(#clawdy-grad)"/>
      <circle cx="14" cy="14" r="3.2" fill="white" opacity="0.95"/>
      <path d="M14 10.8V7.5M14 20.5V17.2M10.8 14H7.5M20.5 14H17.2" stroke="white" stroke-width="1.6" stroke-linecap="round" opacity="0.9"/>
      <path d="M10.2 10.2L8.2 8.2M19.8 19.8L17.8 17.8M19.8 10.2L17.8 8.2M10.2 19.8L8.2 17.8" stroke="white" stroke-width="1.4" stroke-linecap="round" opacity="0.55"/>
      <defs>
        <linearGradient id="clawdy-grad" x1="4" y1="3" x2="24" y2="25" gradientUnits="userSpaceOnUse">
          <stop stop-color="#5856D6"/>
          <stop offset="0.55" stop-color="#4B6BFF"/>
          <stop offset="1" stop-color="#007AFF"/>
        </linearGradient>
      </defs>
    </svg>`;
  },

  providers: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><rect x="2" y="2" width="5" height="5" rx="1.2" stroke="currentColor" stroke-width="1.35"/><rect x="9" y="2" width="5" height="5" rx="1.2" stroke="currentColor" stroke-width="1.35"/><rect x="2" y="9" width="5" height="5" rx="1.2" stroke="currentColor" stroke-width="1.35"/><rect x="9" y="9" width="5" height="5" rx="1.2" stroke="currentColor" stroke-width="1.35"/></svg>',

  conversations: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><path d="M3 3.5h10a1 1 0 011 1v5.5a1 1 0 01-1 1H7l-2.5 2v-2H3a1 1 0 01-1-1V4.5a1 1 0 011-1z" stroke="currentColor" stroke-width="1.35" stroke-linejoin="round"/></svg>',

  monitor: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><path d="M2.5 11V5.5A1.5 1.5 0 014 4h8a1.5 1.5 0 011.5 1.5V11" stroke="currentColor" stroke-width="1.35"/><path d="M5 12.5h6M8 12.5V14" stroke="currentColor" stroke-width="1.35" stroke-linecap="round"/><path d="M5 9l2-2 2 2 3-3.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/></svg>',

  settings: '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M9.594 3.94c.09-.542.56-.94 1.11-.94h2.593c.55 0 1.02.398 1.11.94l.213 1.281c.063.374.313.686.645.87.074.04.147.083.22.127.324.196.72.257 1.075.124l1.217-.456a1.125 1.125 0 0 1 1.37.49l1.296 2.247a1.125 1.125 0 0 1-.26 1.431l-1.003.827c-.293.241-.438.613-.43.992a7.723 7.723 0 0 1 0 .255c-.008.378.137.75.43.991l1.004.827c.424.35.534.955.26 1.43l-1.298 2.247a1.125 1.125 0 0 1-1.369.491l-1.217-.456c-.355-.133-.75-.072-1.076.124a6.47 6.47 0 0 1-.22.128c-.331.183-.581.495-.644.869l-.213 1.281c-.09.543-.56.94-1.11.94h-2.594c-.55 0-1.019-.398-1.11-.94l-.213-1.281c-.062-.374-.312-.686-.644-.87a6.52 6.52 0 0 1-.22-.127c-.325-.196-.72-.257-1.076-.124l-1.217.456a1.125 1.125 0 0 1-1.369-.49l-1.297-2.247a1.125 1.125 0 0 1 .26-1.431l1.004-.827c.292-.24.437-.613.43-.991a6.932 6.932 0 0 1 0-.255c.007-.38-.138-.751-.43-.992l-1.004-.827a1.125 1.125 0 0 1-.26-1.43l1.297-2.247a1.125 1.125 0 0 1 1.37-.491l1.216.456c.356.133.751.072 1.076-.124.072-.044.146-.086.22-.128.332-.183.582-.495.644-.869l.214-1.28Z"/><circle cx="12" cy="12" r="3"/></svg>',

  plus: '<svg width="14" height="14" viewBox="0 0 14 14" fill="none"><path d="M7 2.5v9M2.5 7h9" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/></svg>',

  chevronLeft: '<svg width="12" height="12" viewBox="0 0 12 12" fill="none"><path d="M7.5 2L4 6l3.5 4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>',

  chevronRight: '<svg width="12" height="12" viewBox="0 0 12 12" fill="none"><path d="M4.5 2L8 6l-3.5 4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>',

  // Theme toggle. `theme` is the sun (light mode); `moon` is shown in dark mode — applyTheme() swaps them.
  theme: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4.2"/><path d="M12 2.5v2.2M12 19.3v2.2M4.6 4.6l1.6 1.6M17.8 17.8l1.6 1.6M2.5 12h2.2M19.3 12h2.2M4.6 19.4l1.6-1.6M17.8 6.2l1.6-1.6"/></svg>',

  moon: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>',

  connect: '<svg width="18" height="18" viewBox="0 0 18 18" fill="none"><circle cx="9" cy="9" r="6.5" stroke="currentColor" stroke-width="1.6"/><path d="M9 5.5v7M5.5 9h7" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/></svg>',

  connected: '<svg width="18" height="18" viewBox="0 0 18 18" fill="none"><circle cx="9" cy="9" r="6.5" stroke="currentColor" stroke-width="1.6"/><path d="M6 9.2l2.2 2.2L12.5 7" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>',

  refresh: '<svg width="14" height="14" viewBox="0 0 14 14" fill="none"><path d="M11.5 7a4.5 4.5 0 1 1-1.35-3.15L11.5 5M11.5 2v3H8.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/></svg>',

  edit: '<svg width="14" height="14" viewBox="0 0 14 14" fill="none"><path d="M9.2 2.5l2.3 2.3-6.5 6.5H2.7V9L9.2 2.5z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/></svg>',

  trash: '<svg width="14" height="14" viewBox="0 0 14 14" fill="none"><path d="M3 4.5h8M5.5 4.5V3.5h3v1M5 6v4.5M7 6v4.5M9 6v4.5M4.5 11h5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/></svg>',

  search: '<svg width="14" height="14" viewBox="0 0 14 14" fill="none"><circle cx="6.2" cy="6.2" r="3.5" stroke="currentColor" stroke-width="1.3"/><path d="M9 9l2.5 2.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/></svg>',

  empty: '<svg width="40" height="40" viewBox="0 0 40 40" fill="none"><rect x="6" y="10" width="28" height="20" rx="3" stroke="currentColor" stroke-width="1.5" opacity="0.35"/><path d="M14 18h12M14 23h8" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" opacity="0.25"/></svg>',

  /* settings sub-nav */
  gateway: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><rect x="2.5" y="2.5" width="11" height="4.2" rx="1.2" stroke="currentColor" stroke-width="1.3"/><rect x="2.5" y="9.3" width="11" height="4.2" rx="1.2" stroke="currentColor" stroke-width="1.3"/><path d="M5 4.6h.01M5 11.4h.01" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/></svg>',

  desktop: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><rect x="2" y="3" width="12" height="8" rx="1.3" stroke="currentColor" stroke-width="1.3"/><path d="M6 13.5h4M8 11v2.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/></svg>',

  shield: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><path d="M8 2l4.5 1.8v3.1c0 3-2 5.3-4.5 6.3-2.5-1-4.5-3.3-4.5-6.3V3.8L8 2z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><path d="M6 8l1.4 1.4L10.4 6.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/></svg>',

  sliders: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><path d="M3 5h5M11.6 5H13M3 11h1.4M8 11h5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/><circle cx="9.6" cy="5" r="1.7" stroke="currentColor" stroke-width="1.3"/><circle cx="6" cy="11" r="1.7" stroke="currentColor" stroke-width="1.3"/></svg>',

  folder: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><path d="M2.5 4.6a1 1 0 011-1h2.4l1.2 1.4h5.4a1 1 0 011 1v5.4a1 1 0 01-1 1h-9a1 1 0 01-1-1V4.6z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/></svg>',

  download: '<svg width="14" height="14" viewBox="0 0 16 16" fill="none"><path d="M8 2.5v7M5 6.5l3 3 3-3" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/><path d="M3 11.5v1a1 1 0 001 1h8a1 1 0 001-1v-1" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/></svg>',
};