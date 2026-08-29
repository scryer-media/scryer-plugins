# ameNZB Indexer

An anime-focused Usenet indexer for ameNZB's Newznab API. It supports recent and RSS feeds plus automatic and interactive searches for anime, series, and movies. Anime lookups can use AniDB, TVDB, or an exact torrent info hash.

## Configure in Scryer

Enter your **api_key**. ameNZB pins keys to the caller IP. The prefilled **base_url** is the official service URL and normally does not need changing.

The plugin keeps its provider details internal: it uses ameNZB's `/api` endpoint, the anime Newznab category, 50 results per page, and conservative local request budgets. It deliberately does not apply permanent upstream language, translation, source, resolution, or release-group filters; Scryer's normal profiles and scoring make those decisions after results are retrieved.

## Behavior and limits

Searches are paced and retry rate-limit responses, but stop with no results once the local hit budget is exhausted. A search uses at most two API pages and never asks ameNZB for more than 100 entries per page. Results retain provider metadata such as language, grabs, comments, and info URLs; Scryer still makes the final release and download-client decision.
