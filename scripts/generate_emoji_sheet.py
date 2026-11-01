"""Build the runtime atlas from Noto Emoji; requires Pillow with RAQM.

Pass the unmodified NotoColorEmoji.ttf identified by assets/emoji-source.json.
No system fonts, account data or network access are used by this generator.
"""
from pathlib import Path
import argparse, hashlib, math
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--font', type=Path, required=True)
    parser.add_argument('--output', type=Path, default=ROOT/'assets/emoji-sheet.png')
    args = parser.parse_args()
    import json
    metadata = json.loads((ROOT/'assets/emoji-source.json').read_text(encoding='utf-8'))
    if hashlib.sha256(args.font.read_bytes()).hexdigest() != metadata['font_sha256']:
        raise SystemExit('Font does not match the pinned upstream SHA-256')
    settings = {}
    for line in (ROOT/'assets/emoji-sheet.txt').read_text(encoding='utf-8-sig').splitlines():
        if line and not line.startswith('#') and '=' in line:
            key,value=line.split('=',1)
            settings[key]=value
    def points(ranges):
        for item in ranges.split(','):
            first,last=item.split('-')
            yield from range(int(first,16),int(last,16)+1)
    plain = [chr(c) for c in points(settings['PLAIN'])]
    variants = [chr(c)+'\ufe0f' for c in points(settings['VS'])]
    sequences = [''.join(chr(int(c,16)) for c in item.split('-')) for item in settings['SEQ'].split(',')]
    assert len(plain)==int(settings['PLAINCOUNT'])
    assert len(variants)==int(settings['VSCOUNT'])
    assert len(sequences)==int(settings['SEQCOUNT'])
    assert int(settings['FLAGCOUNT'])==0
    strings = plain+variants+sequences
    cell,columns=int(settings['CELL']),int(settings['COLUMNS'])
    atlas=Image.new('RGBA',(cell*columns,cell*math.ceil(len(strings)/columns)))
    font=ImageFont.truetype(str(args.font),109,layout_engine=ImageFont.Layout.RAQM)
    rendered=0
    for index,text in enumerate(strings):
        tile=Image.new('RGBA',(256,192))
        ImageDraw.Draw(tile).text((0,0),text,font=font,embedded_color=True)
        bbox=tile.getbbox()
        if not bbox: continue
        glyph=tile.crop(bbox)
        glyph.thumbnail((cell-2,cell-2),Image.Resampling.LANCZOS)
        atlas.alpha_composite(glyph,((index%columns)*cell+(cell-glyph.width)//2,(index//columns)*cell+(cell-glyph.height)//2))
        rendered+=1
    args.output.parent.mkdir(parents=True,exist_ok=True)
    atlas.save(args.output,optimize=True)
    print({'cells':len(strings),'rendered':rendered,'size':atlas.size,'sha256':hashlib.sha256(args.output.read_bytes()).hexdigest()})

if __name__=='__main__': main()
