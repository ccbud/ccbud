'use strict';

/*
 * window.I18n — renderer-side i18n runtime (loaded in index.html AND popover.html, after
 * i18n-dict.js). No build step. All 5 supported locales (en/zh/zh-TW/ja/ko) are LTR — there
 * is NO RTL handling here on purpose; adding Arabic/Hebrew later must be a deliberate change.
 */
(function () {
  var D = (window.ClawdyI18nDict) || { DICT: { en: {} }, LANGS: ['en'], LOCALE_TAG: { en: 'en-US' } };
  var lang = 'en';

  function dict() { return D.DICT[lang] || D.DICT.en || {}; }

  function fill(s, params) {
    if (!params) return s;
    return s.replace(/\{(\w+)\}/g, function (_, k) { return params[k] != null ? params[k] : '{' + k + '}'; });
  }

  function t(key, params) {
    var s = dict()[key];
    if (s == null) s = (D.DICT.en && D.DICT.en[key] != null) ? D.DICT.en[key] : key; // fallback: lang → en → key
    return fill(s, params);
  }

  function apply(root) {
    root = root || document;
    root.querySelectorAll('[data-i18n]').forEach(function (el) { el.textContent = t(el.getAttribute('data-i18n')); });
    root.querySelectorAll('[data-i18n-placeholder]').forEach(function (el) { el.setAttribute('placeholder', t(el.getAttribute('data-i18n-placeholder'))); });
    root.querySelectorAll('[data-i18n-title]').forEach(function (el) {
      var v = t(el.getAttribute('data-i18n-title'));
      el.setAttribute('title', v);
      el.setAttribute('aria-label', v);
    });
  }

  function setLang(l) {
    lang = (D.LANGS.indexOf(l) >= 0) ? l : 'en';
    try { document.documentElement.setAttribute('lang', I18n.localeTag); } catch (_) {}
    try { localStorage.setItem('clawdy-lang', lang); } catch (_) {}
  }

  var I18n = {
    t: t,
    apply: apply,
    setLang: setLang,
    has: function (key) { return dict()[key] != null || (D.DICT.en && D.DICT.en[key] != null); },
    get lang() { return lang; },
    get localeTag() { return (D.LOCALE_TAG[lang]) || 'en-US'; },
  };
  window.I18n = I18n;
})();
