ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [ReferenceView]'),'2;1');
FILE_NAME('{{file_name}}','{{timestamp_iso}}',('{{author}}'),('{{organization}}'),'{{originating_system}}','{{preprocessor_version}}','');
FILE_SCHEMA(('{{schema_name}}'));
ENDSEC;

DATA;
#1=IFCPROJECT('{{project_guid}}',#2,'{{project_name}}',$,$,$,$,(#12),#16);
#2=IFCOWNERHISTORY(#3,#6,$,.ADDED.,{{timestamp_unix}},#3,#6,{{timestamp_unix}});
#3=IFCPERSONANDORGANIZATION(#4,#5,$);
#4=IFCPERSON($,'{{author}}',$,$,$,$,$,$);
#5=IFCORGANIZATION($,'{{organization}}',$,$,$);
#6=IFCAPPLICATION(#7,'{{application_version}}','ifc-language-server','ifc-language-server');
#7=IFCORGANIZATION($,'ifc-language-server',$,$,$);
#8=IFCCARTESIANPOINT((0.,0.,0.));
#9=IFCDIRECTION((0.,0.,1.));
#10=IFCDIRECTION((1.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#8,#9,#10);
#12=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,$,#11,$);
#13=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);
#14=IFCSIUNIT(*,.AREAUNIT.,$,.SQUARE_METRE.);
#15=IFCSIUNIT(*,.VOLUMEUNIT.,$,.CUBIC_METRE.);
#16=IFCUNITASSIGNMENT((#13,#14,#15));
{{final_tabstop}}
ENDSEC;
END-ISO-10303-21;
