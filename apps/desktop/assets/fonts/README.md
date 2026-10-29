# Workbench font

`NotoSansSC-VF.ttf` is the original OFL font; its default weight axis is 100.
The UI embeds `NotoSansSC-Regular.ttf`, a static instance pinned to weight 400.
This keeps Chinese menu, dialog and button text readable in egui, which uses
the source font's default axis. Both files are covered by `OFL.txt`.

The terminal renderer also embeds the same regular face as its CJK fallback.
The UI references those shared bytes, so Chinese terminal output remains
available when system font discovery cannot find a CJK font.

Reproduce the static instance with fontTools:

```python
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

font = TTFont("NotoSansSC-VF.ttf")
instantiateVariableFont(font, {"wght": 400}, inplace=True)
font.save("NotoSansSC-Regular.ttf")
```
