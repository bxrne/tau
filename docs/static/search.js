(function () {
  function initSearch() {
    var input = document.getElementById("search-input");
    var results = document.getElementById("search-results");

    if (!input || !results) {
      return;
    }

    if (typeof elasticlunr === "undefined" || !window.searchIndex) {
      results.textContent = "Search index not available.";
      return;
    }

    var index = elasticlunr.Index.load(window.searchIndex);

    function clearResults() {
      results.innerHTML = "";
    }

    function renderResults(docs) {
      if (!docs.length) {
        results.innerHTML = "<p class=\"search-empty\">No results.</p>";
        return;
      }

      var list = document.createElement("ul");
      list.className = "search-list";

      docs.forEach(function (doc) {
        var item = document.createElement("li");
        var link = document.createElement("a");
        var title = document.createElement("div");
        var snippet = document.createElement("div");

        link.href = doc.id;
        link.textContent = doc.title || doc.id;
        title.className = "search-title";
        title.appendChild(link);

        snippet.className = "search-snippet";
        snippet.textContent = (doc.body || "").split("\n")[0];

        item.appendChild(title);
        item.appendChild(snippet);
        list.appendChild(item);
      });

      results.innerHTML = "";
      results.appendChild(list);
    }

    function search(query) {
      var trimmed = query.trim();
      if (!trimmed) {
        clearResults();
        return;
      }

      var matches = index.search(trimmed, { expand: true });
      var docs = matches.map(function (match) {
        return index.documentStore.getDoc(match.ref);
      });

      renderResults(docs);
    }

    input.addEventListener("input", function (event) {
      search(event.target.value);
    });

    if (input.value) {
      search(input.value);
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", initSearch);
  } else {
    initSearch();
  }
})();
