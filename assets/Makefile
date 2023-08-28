all: state-diagram.svg login-state-diagram.svg command-loop-state-diagram.svg play-state-diagram.svg

%.svg: %.mmd
	mmdc -i $< -o tmp.svg
	cat tmp.svg | sed -E 's|width="100%" ||' | sed -E 's|height="[0-9]+" style="max-width: [0-9.]+px;" ||' | sed 's|<br>|<br/>|g' > $@
	rm tmp.svg
