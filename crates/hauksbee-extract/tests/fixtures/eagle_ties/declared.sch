<?xml version="1.0"?>
<eagle version="6.6.0"><drawing><schematic><libraries><library name="supply1" urn="urn:adsk.eagle:library:1"><symbols>
<symbol name="GND"><pin name="GND" direction="sup"/></symbol><symbol name="AGND"><pin name="AGND" direction="sup"/></symbol>
</symbols><devicesets><deviceset name="GND"><gates><gate name="GND" symbol="GND"/></gates></deviceset><deviceset name="AGND"><gates><gate name="AGND" symbol="AGND"/></gates></deviceset></devicesets></library></libraries>
<parts><part name="SUPPLY1" library="supply1" library_urn="urn:adsk.eagle:library:1" deviceset="GND"/><part name="AGND1" library="supply1" library_urn="urn:adsk.eagle:library:1" deviceset="AGND"/></parts>
<sheets><sheet><nets><net name="GND"><segment><pinref part="SUPPLY1"/><pinref part="AGND1"/></segment></net></nets></sheet></sheets>
</schematic></drawing></eagle>
