<?xml version="1.0"?>
<eagle version="6.6.0"><drawing><schematic><libraries><library name="supply1" urn="urn:adsk.eagle:library:1"><symbols>
<symbol name="GND"><pin name="GND" direction="sup"/></symbol><symbol name="AGND"><pin name="AGND" direction="sup"/></symbol>
</symbols><devicesets><deviceset name="GND"><gates><gate name="GND" symbol="GND"/></gates></deviceset><deviceset name="AGND"><gates><gate name="AGND" symbol="AGND"/></gates></deviceset></devicesets></library>
<library name="device"><symbols><symbol name="R"><pin name="1"/><pin name="2"/></symbol></symbols><devicesets><deviceset name="R"><gates><gate name="G$1" symbol="R"/></gates><devices><device name="" package="R0603"><connects><connect gate="G$1" pin="1" pad="1"/><connect gate="G$1" pin="2" pad="2"/></connects></device></devices></deviceset></devicesets></library></libraries>
<parts><part name="SUPPLY1" library="supply1" library_urn="urn:adsk.eagle:library:1" deviceset="GND"/><part name="AGND1" library="supply1" library_urn="urn:adsk.eagle:library:1" deviceset="AGND"/><part name="R1" library="device" deviceset="R" device="" value="10k"/></parts>
<sheets><sheet><nets><net name="GND"><segment><pinref part="SUPPLY1"/><pinref part="AGND1"/><pinref part="R1" gate="G$1" pin="1"/></segment></net><net name="AGND"><segment><pinref part="R1" gate="G$1" pin="2"/></segment></net></nets></sheet></sheets>
</schematic></drawing></eagle>
