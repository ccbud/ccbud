export function injectScript(projectId) {
  try {
    (function (c, l, a, r, i, t, y) {
      if (l.getElementById("clarity-script")) {
        return;
      }
      c[a] = c[a] ||
        function () {
          (c[a].q = c[a].q || []).push(arguments);
        };
      t = l.createElement(r);
      t.async = 1;
      t.src = "https://www.clarity.ms/tag/" + i + "?ref=npm";
      t.id = "clarity-script"
      y = l.getElementsByTagName(r)[0];
      y.parentNode.insertBefore(t, y);
    })(window, document, "clarity", "script", projectId);
    return;
  } catch (error) {
    return;
  }
};
