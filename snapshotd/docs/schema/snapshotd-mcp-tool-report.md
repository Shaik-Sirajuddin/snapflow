# snapshotd MCP tool schema report

Generated from `docs/schema/snapshotd-mcp.schema.json` (76 tools), produced by `snapshotd/cmd/gen-mcp-schema` (see `scripts/gen_mcp_schema.sh`). Reflects every tool this package can build (`mcpadapter.AllTools`) -- the 9 tools `mcpadapter.New` currently wires into the live server (`daemon.*`, `sap.call`, `sap.search`) plus the 67 typed per-method tools in `tools_*.go` that are defined in source but left unregistered on the live server in favor of the generic `sap.call` passthrough.

## Table 1 -- Tool categories (by name prefix)

| Category | # Tools | What it covers | Tools |
|---|---|---|---|
| `edit.*` | 15 | Timeline tracks and clips (add/move/trim/split) | `edit.addTrack`, `edit.appendClip`, `edit.insertClip`, `edit.listClips`, `edit.listTracks`, `edit.moveClip`, `edit.overwriteClip`, `edit.removeClip`, `edit.removeTrack`, `edit.reorderTrack`, `edit.setTrackHeight`, `edit.setTrackProperties`, `edit.splitClip`, `edit.trimClipIn`, `edit.trimClipOut` |
| `markers.*` | 10 | Timeline markers | `markers.append`, `markers.clear`, `markers.get`, `markers.list`, `markers.move`, `markers.next`, `markers.prev`, `markers.remove`, `markers.setColor`, `markers.update` |
| `filter.*` | 8 | MLT filters attached to a clip, incl. keyframes | `filter.add`, `filter.addKeyframe`, `filter.list`, `filter.listKeyframes`, `filter.remove`, `filter.removeKeyframe`, `filter.reorder`, `filter.setProperty` |
| `daemon.*` | 7 | Process/project lifecycle control (create, launch, list, health, close) | `daemon.close`, `daemon.createProject`, `daemon.deleteProject`, `daemon.health`, `daemon.launch`, `daemon.list`, `daemon.listProjects` |
| `playlist.*` | 7 | Project media bin / playlist entries | `playlist.addToTimeline`, `playlist.append`, `playlist.get`, `playlist.insert`, `playlist.list`, `playlist.move`, `playlist.remove` |
| `audio.*` | 6 | Audio-specific filter shortcuts (gain, pan, fades, normalize) | `audio.setAutoFade`, `audio.setBalance`, `audio.setFadeInOut`, `audio.setGain`, `audio.setNormalize`, `audio.setPan` |
| `subtitles.*` | 6 | Subtitle tracks/cues, SRT import/export, burn-in | `subtitles.addTrack`, `subtitles.appendItem`, `subtitles.burnIn`, `subtitles.exportSrt`, `subtitles.importSrt`, `subtitles.removeItems` |
| `jobs.*` | 3 | Export job lifecycle | `jobs.get`, `jobs.list`, `jobs.stop` |
| `recent.*` | 3 | Recently-used path list | `recent.add`, `recent.list`, `recent.remove` |
| `file.*` | 2 | Media import/probe/export at the filesystem level | `file.export`, `file.import` |
| `generator.*` | 2 | Synthetic sources (title cards, color clips) | `generator.createColor`, `generator.createTitle` |
| `notes.*` | 2 | Free-text project notes | `notes.getText`, `notes.setText` |
| `playback.*` | 2 | Playhead seek / frame readback | `playback.getFrame`, `playback.seek` |
| `sap.*` | 2 | Generic passthrough + discovery for every other SAP method | `sap.call`, `sap.search` |
| `transitions.*` | 1 | Cross-clip transitions | `transitions.addCrossfade` |

## Table 2 -- Common input properties (how many tools share each property name)

```ag-note

enterTrack
exitTrack

enterClip
exitClip

current { 
    trackIndex , 
    clipId 
}

mcp applies to current selection, 
cannot apply arbitirary clip edits without mathcing current , 

lets disable arbitirary trackIndex ,clipId in this tool schema to be taken 

must use enter,exit with currentView tool ->  return state current 

its possible some of tools can effect trackIndex of currentTrack ? , like rmeoveTrack tool ?

lets also apply same selection state for filterIndex , whena clipid is elected 
should auto sync current state to change index if index from reorder , remove add changes the index of actual seleciton , so itmaps to original item rather than index


```

| Property | # Tools using it | Tools |
|---|---|---|
| `trackIndex` | 16 | `edit.appendClip`, `edit.insertClip`, `edit.listClips`, `edit.overwriteClip`, `edit.removeClip`, `edit.removeTrack`, `edit.setTrackProperties`, `edit.splitClip`, `edit.trimClipIn`, `edit.trimClipOut`, `playlist.addToTimeline`, `subtitles.appendItem`, `subtitles.burnIn`, `subtitles.exportSrt`, `subtitles.removeItems`, `transitions.addCrossfade` |
| `clipId` | 14 | `audio.setAutoFade`, `audio.setBalance`, `audio.setFadeInOut`, `audio.setGain`, `audio.setNormalize`, `audio.setPan`, `filter.add`, `filter.addKeyframe`, `filter.list`, `filter.listKeyframes`, `filter.remove`, `filter.removeKeyframe`, `filter.reorder`, `filter.setProperty` |
| `position` | 8 | `audio.setBalance`, `audio.setGain`, `audio.setPan`, `edit.splitClip`, `filter.addKeyframe`, `filter.removeKeyframe`, `filter.setProperty`, `playlist.addToTimeline` |
| `clipIndex` | 6 | `edit.insertClip`, `edit.overwriteClip`, `edit.removeClip`, `edit.splitClip`, `edit.trimClipIn`, `edit.trimClipOut` |
| `filterIndex` | 6 | `filter.addKeyframe`, `filter.listKeyframes`, `filter.remove`, `filter.removeKeyframe`, `filter.reorder`, `filter.setProperty` |
| `markerIndex` | 5 | `markers.get`, `markers.move`, `markers.remove`, `markers.setColor`, `markers.update` |
| `path` | 5 | `file.import`, `recent.add`, `recent.remove`, `subtitles.exportSrt`, `subtitles.importSrt` |
| `source` | 5 | `edit.appendClip`, `edit.insertClip`, `edit.overwriteClip`, `playlist.append`, `playlist.insert` |
| `text` | 5 | `generator.createTitle`, `markers.append`, `markers.update`, `notes.setText`, `subtitles.appendItem` |
| `frame` | 4 | `markers.append`, `markers.update`, `playback.getFrame`, `playback.seek` |
| `index` | 4 | `playlist.addToTimeline`, `playlist.get`, `playlist.insert`, `playlist.remove` |
| `property` | 4 | `filter.addKeyframe`, `filter.listKeyframes`, `filter.removeKeyframe`, `filter.setProperty` |
| `color` | 3 | `markers.append`, `markers.setColor`, `markers.update` |
| `name` | 3 | `daemon.createProject`, `playlist.append`, `playlist.insert` |
| `fromFrame` | 2 | `markers.next`, `markers.prev` |
| `fromIndex` | 2 | `edit.reorderTrack`, `playlist.move` |
| `instanceId` | 2 | `daemon.close`, `daemon.health` |
| `jobId` | 2 | `jobs.get`, `jobs.stop` |
| `mode` | 2 | `audio.setNormalize`, `generator.createTitle` |
| `newFrame` | 2 | `edit.trimClipIn`, `edit.trimClipOut` |
| `projectId` | 2 | `daemon.deleteProject`, `daemon.launch` |
| `ripple` | 2 | `edit.trimClipIn`, `edit.trimClipOut` |
| `toIndex` | 2 | `edit.reorderTrack`, `playlist.move` |
| `value` | 2 | `filter.addKeyframe`, `filter.setProperty` |

42 more properties are used by exactly one tool each (omitted above for brevity -- see the schema file directly).

## Table 3 -- Property pairs shared by 2+ tools (co-occurrence)

Property pairs that appear together in the same input schema across multiple tools -- a proxy for "these tools operate on the same kind of struct."

| Property A | Property B | # Tools with both | Tools |
|---|---|---|---|
| `clipId` | `filterIndex` | 6 | `filter.addKeyframe`, `filter.listKeyframes`, `filter.remove`, `filter.removeKeyframe`, `filter.reorder`, `filter.setProperty` |
| `clipId` | `position` | 6 | `audio.setBalance`, `audio.setGain`, `audio.setPan`, `filter.addKeyframe`, `filter.removeKeyframe`, `filter.setProperty` |
| `clipIndex` | `trackIndex` | 6 | `edit.insertClip`, `edit.overwriteClip`, `edit.removeClip`, `edit.splitClip`, `edit.trimClipIn`, `edit.trimClipOut` |
| `clipId` | `property` | 4 | `filter.addKeyframe`, `filter.listKeyframes`, `filter.removeKeyframe`, `filter.setProperty` |
| `filterIndex` | `property` | 4 | `filter.addKeyframe`, `filter.listKeyframes`, `filter.removeKeyframe`, `filter.setProperty` |
| `filterIndex` | `position` | 3 | `filter.addKeyframe`, `filter.removeKeyframe`, `filter.setProperty` |
| `position` | `property` | 3 | `filter.addKeyframe`, `filter.removeKeyframe`, `filter.setProperty` |
| `source` | `trackIndex` | 3 | `edit.appendClip`, `edit.insertClip`, `edit.overwriteClip` |
| `clipId` | `value` | 2 | `filter.addKeyframe`, `filter.setProperty` |
| `clipIndex` | `newFrame` | 2 | `edit.trimClipIn`, `edit.trimClipOut` |
| `clipIndex` | `ripple` | 2 | `edit.trimClipIn`, `edit.trimClipOut` |
| `clipIndex` | `source` | 2 | `edit.insertClip`, `edit.overwriteClip` |
| `color` | `frame` | 2 | `markers.append`, `markers.update` |
| `color` | `markerIndex` | 2 | `markers.setColor`, `markers.update` |
| `color` | `text` | 2 | `markers.append`, `markers.update` |
| `filterIndex` | `value` | 2 | `filter.addKeyframe`, `filter.setProperty` |
| `frame` | `text` | 2 | `markers.append`, `markers.update` |
| `fromIndex` | `toIndex` | 2 | `edit.reorderTrack`, `playlist.move` |
| `name` | `source` | 2 | `playlist.append`, `playlist.insert` |
| `newFrame` | `ripple` | 2 | `edit.trimClipIn`, `edit.trimClipOut` |
| `newFrame` | `trackIndex` | 2 | `edit.trimClipIn`, `edit.trimClipOut` |
| `position` | `trackIndex` | 2 | `edit.splitClip`, `playlist.addToTimeline` |
| `position` | `value` | 2 | `filter.addKeyframe`, `filter.setProperty` |
| `property` | `value` | 2 | `filter.addKeyframe`, `filter.setProperty` |
| `ripple` | `trackIndex` | 2 | `edit.trimClipIn`, `edit.trimClipOut` |

## Table 4 -- Tools with an identical property set ("same struct shape")

Tools whose input schema has exactly the same property names as at least one other tool -- these are the strongest "shared struct" matches (not just a couple of overlapping fields, but the whole shape).

| Shared properties | # Tools | Tools |
|---|---|---|
| `path` | 3 | `file.import`, `recent.add`, `recent.remove` |
| `trackIndex` | 3 | `edit.listClips`, `edit.removeTrack`, `subtitles.burnIn` |
| `clipIndex`, `newFrame`, `ripple`, `trackIndex` | 2 | `edit.trimClipIn`, `edit.trimClipOut` |
| `clipIndex`, `source`, `trackIndex` | 2 | `edit.insertClip`, `edit.overwriteClip` |
| `fromFrame` | 2 | `markers.next`, `markers.prev` |
| `fromIndex`, `toIndex` | 2 | `edit.reorderTrack`, `playlist.move` |
| `index` | 2 | `playlist.get`, `playlist.remove` |
| `instanceId` | 2 | `daemon.close`, `daemon.health` |
| `jobId` | 2 | `jobs.get`, `jobs.stop` |
| `markerIndex` | 2 | `markers.get`, `markers.remove` |
